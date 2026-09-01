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
    providers::{
        Provider,
        RootProvider,
    },
    signers::local::PrivateKeySigner,
};
use axum::Router;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{
    Any,
    CorsLayer,
};
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
    pub ens_source: EnsSource,

    /// JSON-RPC endpoint per chain, required with `ENS_SOURCE=chain`:
    /// `3735928814=http://…,8453=https://…`.
    #[arg(long, env = "ENS_RPC_URLS")]
    pub ens_rpc_urls: Option<String>,

    /// `IdentityNames` per chain, for the chains where it is not at
    /// `IDENTITY_NAMES_ADDRESS`: `8453=0x…`. Only read with
    /// `ENS_SOURCE=chain`.
    #[arg(long, env = "ENS_CONTRACTS")]
    pub ens_contracts: Option<String>,

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

    let gateway = ens_config(&config, chain_id, pool).await?;
    match &gateway {
        Some(g) => info!(
            resolver = %g.resolver,
            route = %format!("{}/{{sender}}/{{data}}", ens::ROUTE_PREFIX),
            "ENS gateway mounted"
        ),
        None => info!("ENS gateway not configured; the CCIP-Read route is absent"),
    }
    let app = build_router(api::AppState::new(store.clone(), contract), gateway);

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
/// Where a signed answer comes from.
///
/// A type rather than a string so clap rejects a typo at startup with the
/// valid values listed, instead of the gateway mounting and every request
/// failing on a `match` arm nothing reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EnsSource {
    /// Read the indexed model. Answers as of the last window, so the lag gate
    /// applies.
    Mirror,
    /// Read `IdentityNames` over RPC. Current rather than as-of-last-window.
    Chain,
}

/// Every route the binary serves, with the middleware over the whole of it.
///
/// Separate from [`run`] so a test can exercise what is actually served. The
/// halves were only ever built apart — `api::router` in one suite,
/// `ens::router` in another — so the merge itself, and the layering over it,
/// had no coverage at all.
pub fn build_router(state: api::AppState, gateway: Option<ens::Config>) -> Router {
    let mut app = api::router(state);
    if let Some(gateway) = gateway {
        app = app.nest(
            ens::ROUTE_PREFIX,
            ens::router(ens::GatewayState::new(gateway)),
        );
    }
    // ONE layer, over the whole router, after every route is on it. A `layer`
    // wraps only what is already mounted, so applying it inside `api::router`
    // both missed the gateway route — a browser ERC-3668 client would get no
    // `Access-Control-Allow-Origin` while `/v1/*` worked from the same page —
    // and, once a second layer was added to cover it, ran the CORS middleware
    // twice on every `/v1` response.
    //
    // A read-only public resolver: any origin may GET. This is what lets a
    // browser UI (handle.link) call the API cross-origin at all.
    app.layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([axum::http::Method::GET]),
    )
}

/// The resolver's `MAX_LIFETIME`, in seconds. It rejects any `expires` further
/// out than this, so a longer TTL here would have the gateway sign answers the
/// contract reverts — every one of them, and only in production, since nothing
/// off chain looks at the deadline.
const RESOLVER_MAX_LIFETIME_SECS: u64 = 3600;

async fn ens_config(
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
    let contracts = parse_contracts(config.ens_contracts.as_deref())?;

    // The resolver enforces this on chain; refuse at startup rather than let a
    // deployment sign answers that always revert.
    if config.ens_ttl_secs > RESOLVER_MAX_LIFETIME_SECS {
        anyhow::bail!(
            "ENS_TTL_SECS is {}, but the resolver's MAX_LIFETIME is {}s and it \
             reverts DeadlineTooFar on anything longer",
            config.ens_ttl_secs,
            RESOLVER_MAX_LIFETIME_SECS
        );
    }

    // Belongs before the loop: an empty `ENS_CHAINS` skips the body entirely,
    // so the gateway used to start cleanly, log "gateway mounted", and then
    // 503 every request because the map was empty.
    if declared.is_empty() {
        anyhow::bail!(
            "ENS_CHAINS names no chain; a gateway serving none answers nothing"
        );
    }

    let mut chains: HashMap<u64, ens::ChainGateway> = HashMap::new();
    for (id, label) in declared {
        // A repeated chain id is invisible to the collision check below, which
        // asks whether two DIFFERENT ids share a coin type. Left alone, the
        // second entry silently overwrites the first, and the label the
        // operator meant to serve is simply gone — every name under it gets a
        // SIGNED null for a chain they believe is configured.
        if chains.contains_key(&id) {
            anyhow::bail!("ENS_CHAINS names chain {id} twice");
        }
        // A label the name grammar can never produce makes the chain
        // unaddressable by label, and says so nowhere. The rules come from
        // `usernames-core` rather than being restated here, so the two cannot
        // drift.
        if let Some(label) = label.as_deref() {
            if !usernames_core::ens::label_is_wellformed(label) {
                anyhow::bail!(
                    "ENS_CHAINS label {label:?} is not valid ENS-normalized text; \
                     a name carrying it cannot parse, so the chain would be \
                     unaddressable by label"
                );
            }
            // The grammar reads right to left and treats the rightmost label
            // as the platform (or the id marker), so a chain label spelled
            // like one is read as that instead — and the chain becomes
            // unreachable rather than ambiguous.
            if usernames_core::nodes::Platform::from_key(label).is_some() {
                anyhow::bail!(
                    "ENS_CHAINS label {label:?} collides with a platform label; \
                     the name grammar would read it as the platform"
                );
            }
        }
        // Two chains whose coin types collide cannot be told apart by a query,
        // so a gateway serving both would answer one's binding for the other.
        // `0x80000000 | chainId` stops being injective past 2^31 — the eden
        // testnet's 3735928814 is such a chain — but serving eden is perfectly
        // safe as long as its colliding twin is not also served. Refused HERE,
        // where both chain ids are known, rather than by banning the chain.
        if let Some(other) = chains
            .keys()
            .copied()
            .find(|other| usernames_core::ens::chain_ids_collide(id, *other))
        {
            anyhow::bail!(
                "chains {id} and {other} share one ENSIP-11 coin type, so a query \
                 cannot say which it means; serve them from separate gateways"
            );
        }
        let source = match config.ens_source {
            EnsSource::Mirror => {
                // A mirror serves whichever chain's rows it is scoped to, and
                // they all live in this one database.
                ens::HandleSource::Mirror(db::ChainStore::new(
                    pool.clone(),
                    i64::try_from(id).map_err(|_| {
                        anyhow::anyhow!("chain id {id} does not fit a BIGINT")
                    })?,
                ))
            }
            EnsSource::Chain => {
                let url = rpc.get(&id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "ENS_SOURCE=chain but ENS_RPC_URLS has no entry for {id}"
                    )
                })?;
                let provider = RootProvider::new_http(url.clone());
                // The signature binds the resolver, the deadline, the request
                // and the result — never the chain. So a transposed
                // `ENS_RPC_URLS` entry (two chains, two URLs, one copy-paste)
                // has this gateway read one chain's bindings and SIGN them
                // under the other's coin type, and the resolver accepts them
                // because nothing in the digest disagrees. A wallet is handed
                // the wrong address, authoritatively, and nothing is logged.
                // Ask the endpoint who it is, as the indexer already does.
                let reported = provider.get_chain_id().await.map_err(|e| {
                    anyhow::anyhow!(
                        "ENS_RPC_URLS entry for chain {id} is unreachable: {e}"
                    )
                })?;
                anyhow::ensure!(
                    reported == id,
                    "ENS_RPC_URLS entry for chain {id} reports chain {reported}; \
                     answers read from it would be signed under the wrong coin type"
                );
                ens::HandleSource::Chain {
                    provider,
                    // Per chain, with `IDENTITY_NAMES_ADDRESS` as the default.
                    // The canonical entry contracts are CREATE3-deterministic
                    // and identical everywhere, but this field is documented
                    // as the ERC1967 PROXY the rows were indexed from, and a
                    // proxy carries no such guarantee. Assuming it does turns
                    // a mismatch into an `eth_call` against an address with no
                    // code, which surfaces as a decode failure rather than as
                    // a configuration error.
                    contract: *contracts
                        .get(&id)
                        .unwrap_or(&config.identity_names_address),
                }
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

/// `8453=0x…,10=0x…`.
fn parse_contracts(raw: Option<&str>) -> anyhow::Result<HashMap<u64, Address>> {
    let Some(raw) = raw else {
        return Ok(HashMap::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (id, address) = entry.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("ENS_CONTRACTS: {entry:?} is not `chainId=address`")
            })?;
            Ok((
                id.trim().parse::<u64>().map_err(|_| {
                    anyhow::anyhow!("ENS_CONTRACTS: {id:?} is not a chain id")
                })?,
                address
                    .trim()
                    .parse::<Address>()
                    .map_err(|e| anyhow::anyhow!("ENS_CONTRACTS: {address:?}: {e}"))?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that never dials. Every case below is refused before any query
    /// runs, which is the point: these are startup checks, so they must not
    /// need a database to reject a configuration that cannot work.
    fn lazy_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool")
    }

    /// A signer key is what turns the route on, so every case supplies one.
    fn config(extra: &[&str]) -> Config {
        let mut argv = vec![
            "usernames-api",
            "--database-url",
            "postgres://u:p@127.0.0.1:5432/db",
            "--identity-names-address",
            "0xd467d48769c26faee36ba6b6fc9228f14aef6dd2",
            "--chain-id",
            "3735928814",
            "--ens-signer-key",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            "--ens-resolver-address",
            "0x0000000000000000000000000000000000000001",
        ];
        argv.extend_from_slice(extra);
        Config::try_parse_from(argv).expect("parse")
    }

    /// `ens::Config` holds a signer and so is not `Debug`; match rather than
    /// `expect_err`.
    async fn refusal(extra: &[&str]) -> String {
        match ens_config(&config(extra), 3735928814, lazy_pool()).await {
            Ok(_) => panic!("this configuration must be refused"),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn without_a_signer_key_there_is_no_gateway() {
        let config = Config::try_parse_from([
            "usernames-api",
            "--database-url",
            "postgres://u:p@127.0.0.1:5432/db",
            "--identity-names-address",
            "0xd467d48769c26faee36ba6b6fc9228f14aef6dd2",
            "--chain-id",
            "1",
        ])
        .expect("parse");
        let mounted = ens_config(&config, 1, lazy_pool())
            .await
            .expect("no key is not an error")
            .is_some();
        assert!(!mounted, "the route must not be mounted without a key");
    }

    #[tokio::test]
    async fn a_ttl_the_resolver_would_reject_is_refused() {
        let message = refusal(&["--ens-ttl-secs", "7200"]).await;
        assert!(message.contains("MAX_LIFETIME"), "{message}");
    }

    #[tokio::test]
    async fn a_ttl_at_the_ceiling_is_allowed() {
        let config = config(&["--ens-ttl-secs", "3600"]);
        assert!(ens_config(&config, 3735928814, lazy_pool()).await.is_ok());
    }

    #[tokio::test]
    async fn a_repeated_chain_is_refused() {
        // Invisible to the collision check, which compares DIFFERENT ids: the
        // second entry used to overwrite the first in silence.
        let message = refusal(&["--ens-chains", "8453:base,8453:eden"]).await;
        assert!(message.contains("twice"), "{message}");
    }

    #[tokio::test]
    async fn a_label_spelled_like_a_platform_is_refused() {
        // The grammar reads right to left, so a trailing `x` is the PLATFORM.
        let message = refusal(&["--ens-chains", "8453:x"]).await;
        assert!(message.contains("collides"), "{message}");
    }

    #[tokio::test]
    async fn a_label_the_grammar_cannot_produce_is_refused() {
        for label in ["Base", "ba_se", "with space"] {
            let message = refusal(&["--ens-chains", &format!("8453:{label}")]).await;
            assert!(
                message.contains("ENS-normalized"),
                "{label:?} was accepted: {message}"
            );
        }
    }

    #[tokio::test]
    async fn a_gateway_serving_no_chain_is_refused() {
        let message = refusal(&["--ens-chains", ""]).await;
        assert!(message.contains("names no chain"), "{message}");
    }

    #[tokio::test]
    async fn colliding_coin_types_are_refused_together() {
        // 3735928814 and 1588445166 differ only in the bit ENSIP-11 sets.
        let message = refusal(&["--ens-chains", "3735928814:eden,1588445166:twin"]).await;
        assert!(message.contains("coin type"), "{message}");
    }

    #[tokio::test]
    async fn chain_source_without_an_endpoint_is_refused() {
        let message = refusal(&["--ens-source", "chain"]).await;
        assert!(message.contains("ENS_RPC_URLS"), "{message}");
    }
}
