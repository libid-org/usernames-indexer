//! The read half: serve resolution and search over the indexed read model,
//! and index nothing.
//!
//! Stateless and horizontal. It takes no writer lease, runs no migration and
//! opens no RPC connection — the chain reaches it only through rows the
//! indexer wrote. Several of these may serve one database. The `/v1` half is
//! scoped to the chain id it was configured with; the ENS gateway serves every
//! chain the store holds, read per request.
//!
//! It answers `503 not_synced` until that chain's first window is committed,
//! which is why readiness must gate on `/v1/status` rather than `/health`.

#![deny(missing_docs)]

pub mod ens;

use std::{
    net::SocketAddr,
    sync::Arc,
};

use alloy::{
    primitives::Address,
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
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
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
    #[arg(long, env = "ENS_SIGNER_KEY", hide_env_values = true)]
    pub ens_signer_key: Option<String>,

    /// The resolver this gateway answers for. Required with a signing key:
    /// the signature binds an answer to one resolver, and signing for a
    /// caller-supplied one would lend this key to any contract that asked.
    #[arg(long, env = "ENS_RESOLVER_ADDRESS")]
    pub ens_resolver_address: Option<Address>,

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

    // Configured BEFORE the port opens: it can refuse to start, and binding
    // first would accept connections the process is not yet able to serve.
    let gateway = config.gateway(pool)?;
    match &gateway {
        Some(g) => info!(
            resolver = %g.resolver,
            route = %format!("{}/{{sender}}/{{data}}", ens::ROUTE_PREFIX),
            "ENS gateway configured"
        ),
        None => info!("ENS gateway not configured; the CCIP-Read route is absent"),
    }
    let app = build_router(api::AppState::new(store.clone(), contract), gateway);

    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, chain_id, %contract, "read API listening");

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

/// How far a block timestamp may trail the wall clock this process reads.
///
/// The gateway dates an answer `now + ttl`, and the resolver compares it to
/// `block.timestamp + MAX_LIFETIME` at whatever block the client's `eth_call`
/// lands on. That block is always at least a little behind now, so a TTL equal
/// to `MAX_LIFETIME` puts `expires` past the ceiling by exactly the amount the
/// chain trails — and every answer reverts, in production only, with nothing
/// off chain the wiser. The room is generous on purpose: shortening a TTL costs
/// a caller nothing, and guessing this too small costs every answer.
const BLOCK_TIMESTAMP_SLACK_SECS: u64 = 300;

/// The longest TTL a gateway may be configured with.
const MAX_TTL_SECS: u64 = RESOLVER_MAX_LIFETIME_SECS - BLOCK_TIMESTAMP_SLACK_SECS;

impl Config {
    /// The key this gateway signs with, and the resolver it signs for.
    ///
    /// `None` when no gateway runs: the key is what turns the route on. A key
    /// without a resolver is a usage error rather than a default, because
    /// there is no safe resolver to guess — signing for whatever address asked
    /// would make this a signing oracle.
    fn signing_identity(&self) -> anyhow::Result<Option<(PrivateKeySigner, Address)>> {
        let Some(key) = self.ens_signer_key.as_deref() else {
            return Ok(None);
        };
        let resolver = self.ens_resolver_address.ok_or_else(|| {
            anyhow::anyhow!("ENS_SIGNER_KEY is set but ENS_RESOLVER_ADDRESS is not")
        })?;
        let signer: PrivateKeySigner = key
            .trim_start_matches("0x")
            .parse()
            .map_err(|e| anyhow::anyhow!("ENS_SIGNER_KEY is not a private key: {e}"))?;
        Ok(Some((signer, resolver)))
    }

    /// The gateway this configuration describes, or `None` when this
    /// deployment runs none.
    pub fn gateway(&self, pool: sqlx::PgPool) -> anyhow::Result<Option<ens::Config>> {
        let Some((signer, resolver)) = self.signing_identity()? else {
            return Ok(None);
        };

        // The resolver enforces this on chain; refuse at startup rather than
        // let a deployment sign answers that always revert.
        if self.ens_ttl_secs > MAX_TTL_SECS {
            anyhow::bail!(
                "ENS_TTL_SECS is {}, but the resolver's MAX_LIFETIME is {}s and it \
                 measures from the block's timestamp, not from now — leave {}s for \
                 the chain to trail, so at most {}",
                self.ens_ttl_secs,
                RESOLVER_MAX_LIFETIME_SECS,
                BLOCK_TIMESTAMP_SLACK_SECS,
                MAX_TTL_SECS
            );
        }

        Ok(Some(ens::Config {
            resolver,
            store: db::Store::new(pool),
            ttl_secs: self.ens_ttl_secs,
            max_lag_blocks: self.ens_max_lag_blocks,
            signer: Arc::new(signer),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that never dials. Every case below is refused before any query
    /// runs, which is the point: these are startup checks, so they must not
    /// need a database to reject a configuration that cannot work. It still
    /// wants a runtime to be built in, which is why the cases are async.
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
    fn refusal(extra: &[&str]) -> String {
        match config(extra).gateway(lazy_pool()) {
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
        let mounted = config
            .gateway(lazy_pool())
            .expect("no key is not an error")
            .is_some();
        assert!(!mounted, "the route must not be mounted without a key");
    }

    #[tokio::test]
    async fn a_ttl_the_resolver_would_reject_is_refused() {
        let message = refusal(&["--ens-ttl-secs", "7200"]);
        assert!(message.contains("MAX_LIFETIME"), "{message}");
    }

    /// The ceiling leaves room for the chain to trail. A TTL equal to the
    /// resolver's `MAX_LIFETIME` is refused, because it dates every answer past
    /// what the contract accepts at any block older than this instant — which
    /// is every block.
    #[tokio::test]
    async fn the_ttl_ceiling_leaves_the_chain_room_to_trail() {
        let at_ceiling = config(&["--ens-ttl-secs", &MAX_TTL_SECS.to_string()]);
        assert!(at_ceiling.gateway(lazy_pool()).is_ok());

        let message =
            refusal(&["--ens-ttl-secs", &RESOLVER_MAX_LIFETIME_SECS.to_string()]);
        assert!(message.contains("trail"), "{message}");
    }
}

#[cfg(test)]
mod help_tests {
    use clap::CommandFactory;

    use super::Config;

    /// clap renders `[env: NAME=value]` in `--help` for every argument bound
    /// to a variable, and `.env` is read before parsing, so without
    /// `hide_env_values` a secret is one `--help` away from a CI log.
    #[test]
    fn help_never_prints_a_secret() {
        let command = Config::command();
        for secret in ["database_url", "ens_signer_key"] {
            let arg = command
                .get_arguments()
                .find(|a| a.get_id() == secret)
                .unwrap_or_else(|| panic!("{secret} is an argument"));
            assert!(
                arg.is_hide_env_values_set(),
                "{secret} shows its value in --help"
            );
        }
    }
}
