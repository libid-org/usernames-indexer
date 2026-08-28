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

use std::net::SocketAddr;

use alloy::primitives::Address;
use clap::Parser;
use tokio_util::sync::CancellationToken;
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
    let store = db::ChainStore::new(pool, chain_id);

    let cancel = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, chain_id, %contract, "read API listening");

    let shutdown = cancel.clone();
    let mut task = tokio::spawn(async move {
        axum::serve(listener, api::router(api::AppState::new(store, contract)))
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
