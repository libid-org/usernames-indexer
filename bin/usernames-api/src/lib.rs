//! The read half: serve resolution and search over the indexed read model,
//! and index nothing.
//!
//! Stateless and horizontal. It takes no writer lease, runs no migration and
//! opens no RPC connection — the chain reaches it only through rows the
//! indexer wrote. Several of these may serve one database, and one of them may
//! serve several chains' worth of rows only in the sense that each process is
//! scoped to the chain id it was configured with.
//!
//! It answers `503 not_synced` until that chain's first window is committed,
//! which is why readiness must gate on `/v1/status` rather than `/health`.

#![deny(missing_docs)]

pub mod ens;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
};

use alloy::{
    primitives::Address,
    providers::RootProvider,
    signers::local::PrivateKeySigner,
};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;
use usernames_core::{
    api,
    db,
};

/// Everything comes from flags or the environment; a `.env` file is read
/// first so local runs need no exports.
///
/// No `RPC_URL`: this process never talks to a chain. No `CONFIRMATIONS`,
/// `POLL_INTERVAL_SECS`, `MAX_BLOCK_RANGE` or `START_BLOCK` either — those are
/// the indexer's, and accepting them here would suggest they did something.
#[derive(Debug, Parser)]
#[command(name = "usernames-api", about)]
pub struct Config {
    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// The IdentityNames ERC1967 proxy this database was indexed from,
    /// echoed in `/v1/status`. It must match the indexer's, or the status a
    /// caller reads names a contract these rows did not come from.
    #[arg(long, env = "IDENTITY_NAMES_ADDRESS")]
    pub identity_names_address: Address,

    /// Which chain's rows to serve. Required here, unlike the indexer, where
    /// the RPC reports it: nothing in this process can discover it.
    #[arg(long, env = "CHAIN_ID")]
    pub chain_id: u64,

    /// Address the read API listens on.
    #[arg(long, env = "LISTEN_ADDR", default_value = "127.0.0.1:8080")]
    pub listen_addr: SocketAddr,

    /// The ENS gateway's signing key, hex. Setting it is what turns the
    /// CCIP-Read route on: unset, the route is not mounted at all rather than
    /// mounted and failing, so a deployment that has not been given a key
    /// serves 404 there instead of 500.
    #[arg(long, env = "ENS_SIGNER_KEY")]
    pub ens_signer_key: Option<String>,

    /// The resolver this gateway answers for. Required with a signing key:
    /// the signature binds an answer to one resolver, and signing for a
    /// caller-supplied one would lend this key to any contract that asked.
    #[arg(long, env = "ENS_RESOLVER_ADDRESS")]
    pub ens_resolver_address: Option<Address>,

    /// Which chains this gateway answers for, and what each is called in the
    /// name hierarchy: `3735928814:eden,8453:base`. The label is optional.
    ///
    /// A coin type naming anything outside this set gets an UNSIGNED refusal,
    /// not a signed null. The resolver carries one `urls` list for every chain
    /// it ever answers, so a signed null here would end the client's walk
    /// through that list — and a sibling gateway further down it may have been
    /// the one that could answer.
    ///
    /// Unset falls back to the chain this process indexes, which is the
    /// single-chain deployment.
    #[arg(long, env = "ENS_CHAINS")]
    pub ens_chains: Option<String>,

    /// Where answers come from: `mirror` reads the indexed model, `chain`
    /// reads `IdentityNames` over RPC.
    ///
    /// `chain` is the stronger source for a SIGNED answer — it is current
    /// rather than as-of-last-window, so `ENS_MAX_LAG_BLOCKS` stops applying.
    /// `mirror` is the default because it needs no further configuration and
    /// because it answers from a model kept `CONFIRMATIONS` blocks deep, which
    /// a call at the head is not.
    #[arg(long, env = "ENS_SOURCE", default_value = "mirror")]
    pub ens_source: String,

    /// JSON-RPC endpoint per chain, required with `ENS_SOURCE=chain`:
    /// `3735928814=http://…,8453=https://…`.
    #[arg(long, env = "ENS_RPC_URLS")]
    pub ens_rpc_urls: Option<String>,

    /// How long a signed answer stays good, in seconds. The resolver enforces
    /// it on chain.
    #[arg(long, env = "ENS_TTL_SECS", default_value_t = 300)]
    pub ens_ttl_secs: u64,

    /// How far behind the chain the mirror may be and still assert anything.
    /// Past it the gateway refuses UNSIGNED rather than signing a null: a
    /// signed null is an authoritative "nobody holds this", and a stale
    /// mirror has not earned the right to say that.
    #[arg(long, env = "ENS_MAX_LAG_BLOCKS", default_value_t = 32)]
    pub ens_max_lag_blocks: u64,
}

/// Parse the environment, connect, and serve until ctrl-c.
pub async fn run() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::parse();
    let contract = config.identity_names_address;
    let chain_id = i64::try_from(config.chain_id).map_err(|_| {
        anyhow::anyhow!("chain id {} does not fit in a BIGINT", config.chain_id)
    })?;

    // `connect`, not `connect_and_migrate`: the schema belongs to the writer.
    let pool = db::connect(&config.database_url).await?;
    let store = db::ChainStore::new(pool.clone(), chain_id);

    let cancel = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, chain_id, %contract, "read API listening");

    let mut app = api::router(api::AppState::new(store.clone(), contract));
    match ens_config(&config, chain_id, pool)? {
        Some(gateway) => {
            info!(resolver = %gateway.resolver, "ENS gateway mounted");
            app = app.merge(ens::router(ens::GatewayState::new(gateway)));
        }
        None => info!("ENS gateway not configured; the CCIP-Read route is absent"),
    }

    let shutdown = cancel.clone();
    let mut task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
    });

    tokio::select! {
        r = tokio::signal::ctrl_c() => {
            info!("shutting down");
            cancel.cancel();
            let _ = task.await;
            r?;
            Ok(())
        }
        r = &mut task => match r {
            Ok(Ok(())) => Err(anyhow::anyhow!("API task exited unexpectedly")),
            Ok(Err(e)) => Err(anyhow::anyhow!("API server failed: {e}")),
            Err(e) => Err(anyhow::anyhow!("API task died: {e}")),
        },
    }
}

/// Build the gateway configuration, or `None` when this deployment does not
/// run one.
///
/// A key without a resolver is a usage error rather than a default, because
/// there is no safe resolver to guess: signing for whatever address asks would
/// make this a signing oracle.
fn ens_config(
    config: &Config,
    chain_id: i64,
    pool: sqlx::PgPool,
) -> anyhow::Result<Option<ens::Config>> {
    let Some(key) = config.ens_signer_key.as_deref() else {
        return Ok(None);
    };
    let resolver = config.ens_resolver_address.ok_or_else(|| {
        anyhow::anyhow!("ENS_SIGNER_KEY is set but ENS_RESOLVER_ADDRESS is not")
    })?;
    let signer: PrivateKeySigner = key
        .trim_start_matches("0x")
        .parse()
        .map_err(|e| anyhow::anyhow!("ENS_SIGNER_KEY is not a private key: {e}"))?;

    let own = u64::try_from(chain_id).expect("chain id came from a u64");
    let declared = parse_chains(config.ens_chains.as_deref(), own)?;
    let rpc = parse_rpc_urls(config.ens_rpc_urls.as_deref())?;

    let mut chains = HashMap::new();
    for (id, label) in declared {
        let source = match config.ens_source.as_str() {
            "mirror" => {
                // A mirror serves whichever chain's rows it is scoped to, and
                // they all live in this one database.
                ens::HandleSource::Mirror(db::ChainStore::new(
                    pool.clone(),
                    i64::try_from(id).map_err(|_| {
                        anyhow::anyhow!("chain id {id} does not fit a BIGINT")
                    })?,
                ))
            }
            "chain" => {
                let url = rpc.get(&id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "ENS_SOURCE=chain but ENS_RPC_URLS has no entry for {id}"
                    )
                })?;
                ens::HandleSource::Chain {
                    provider: RootProvider::new_http(url.clone()),
                    // One CREATE3 address on every chain, which is what lets
                    // a single configured address serve all of them.
                    contract: config.identity_names_address,
                }
            }
            other => {
                anyhow::bail!("ENS_SOURCE must be `mirror` or `chain`, not {other:?}")
            }
        };
        chains.insert(id, ens::ChainGateway { label, source });
    }

    Ok(Some(ens::Config {
        resolver,
        chains,
        ttl_secs: config.ens_ttl_secs,
        max_lag_blocks: config.ens_max_lag_blocks,
        signer: Arc::new(signer),
    }))
}

/// `3735928814:eden,8453:base`, or the process's own chain when unset.
fn parse_chains(
    raw: Option<&str>,
    own: u64,
) -> anyhow::Result<Vec<(u64, Option<String>)>> {
    let Some(raw) = raw else {
        return Ok(vec![(own, None)]);
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (id, label) = match entry.split_once(':') {
                Some((id, label)) => (id, Some(label.trim().to_string())),
                None => (entry, None),
            };
            Ok((
                id.trim().parse::<u64>().map_err(|_| {
                    anyhow::anyhow!("ENS_CHAINS: {id:?} is not a chain id")
                })?,
                label,
            ))
        })
        .collect()
}

/// `3735928814=http://…,8453=https://…`.
fn parse_rpc_urls(raw: Option<&str>) -> anyhow::Result<HashMap<u64, Url>> {
    let Some(raw) = raw else {
        return Ok(HashMap::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (id, url) = entry.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("ENS_RPC_URLS: {entry:?} is not `chainId=url`")
            })?;
            Ok((
                id.trim().parse::<u64>().map_err(|_| {
                    anyhow::anyhow!("ENS_RPC_URLS: {id:?} is not a chain id")
                })?,
                url.trim().parse::<Url>().map_err(|e| {
                    anyhow::anyhow!("ENS_RPC_URLS: {url:?} is not a URL: {e}")
                })?,
            ))
        })
        .collect()
}
