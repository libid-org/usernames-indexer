//! Configuration and startup: one process runs the indexing loop and the read
//! API, and either one dying takes the process down so the supervisor
//! restarts both.

use std::net::SocketAddr;

use alloy::{
    primitives::Address,
    providers::{
        Provider,
        RootProvider,
    },
};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

use crate::{
    api,
    db,
    indexer,
};

/// Everything comes from flags or the environment; a `.env` file is read
/// first so local runs need no exports.
#[derive(Debug, Parser)]
#[command(name = "usernames-indexer", about)]
pub struct Config {
    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// JSON-RPC endpoint of the chain to follow.
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Url,

    /// The IdentityNames ERC1967 proxy address. Typed as an address so
    /// garbage is a usage error before anything connects, not a mid-startup
    /// failure.
    #[arg(long, env = "IDENTITY_NAMES_ADDRESS")]
    pub identity_names_address: Address,

    /// Refuse to start unless the RPC reports this chain id. Optional, but a
    /// deployment that sets it cannot silently index the wrong chain.
    #[arg(long, env = "CHAIN_ID")]
    pub chain_id: Option<u64>,

    /// Blocks behind the head to stay; shallow-reorg protection.
    #[arg(long, env = "CONFIRMATIONS", default_value_t = 5)]
    pub confirmations: u64,

    /// Seconds between poll cycles, and the retry delay after a failure.
    #[arg(long, env = "POLL_INTERVAL_SECS", default_value_t = 5)]
    pub poll_interval_secs: u64,

    /// Largest eth_getLogs window, sized to the RPC provider's limits.
    #[arg(long, env = "MAX_BLOCK_RANGE", default_value_t = 10_000)]
    pub max_block_range: u64,

    /// Scan start override. Unset means: detect the deployment block.
    #[arg(long, env = "START_BLOCK")]
    pub start_block: Option<u64>,

    /// Address the read API listens on.
    #[arg(long, env = "LISTEN_ADDR", default_value = "127.0.0.1:8080")]
    pub listen_addr: SocketAddr,
}

/// Parse the environment, connect everything, and run until ctrl-c.
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

    let provider: RootProvider = RootProvider::new_http(config.rpc_url.clone());
    let reported = provider.get_chain_id().await?;
    if let Some(expected) = config.chain_id {
        anyhow::ensure!(
            reported == expected,
            "RPC reports chain id {reported}, config expects {expected}"
        );
    }
    let chain_id = i64::try_from(reported)
        .map_err(|_| anyhow::anyhow!("chain id {reported} does not fit in a BIGINT"))?;

    let pool = db::connect_and_migrate(&config.database_url).await?;
    let store = db::ChainStore::new(pool, chain_id);
    // The writer lease outlives everything below: as long as this process
    // may write the chain, no other instance may. prepare runs under it —
    // the lease is a parameter there, so a version-bump replay cannot race a
    // still-running older pod.
    let writer = store.acquire_writer().await?;
    store.prepare(&writer, contract).await?;

    let cancel = CancellationToken::new();

    let indexer_config = indexer::IndexerConfig {
        contract,
        confirmations: config.confirmations,
        poll_interval_secs: config.poll_interval_secs,
        max_block_range: config.max_block_range,
        start_block: config.start_block,
    };
    let indexer = indexer::Indexer::new(store.clone(), provider, indexer_config);
    let indexer_task = tokio::spawn(indexer.run(cancel.clone()));

    let state = api::AppState::new(store, contract);
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, "read API listening");
    let api_cancel = cancel.clone();
    let api_task = tokio::spawn(async move {
        axum::serve(listener, api::router(state))
            .with_graceful_shutdown(async move { api_cancel.cancelled().await })
            .await
    });

    // Exit on the first of: ctrl-c, the indexer dying, the API dying. A
    // panicked half must take the process with it — a supervisor restarts a
    // dead process, but nothing notices an indexer that stopped advancing
    // behind a healthy-looking API. Each branch awaits only the OTHER task
    // after cancelling: the branch's own handle has already run to
    // completion, and a JoinHandle must not be polled twice.
    let mut indexer_task = indexer_task;
    let mut api_task = api_task;
    tokio::select! {
        r = tokio::signal::ctrl_c() => {
            info!("shutting down");
            cancel.cancel();
            let _ = indexer_task.await;
            let _ = api_task.await;
            r?;
            Ok(())
        }
        r = &mut indexer_task => {
            cancel.cancel();
            let _ = api_task.await;
            match r {
                Ok(()) => Err(anyhow::anyhow!("indexer task exited unexpectedly")),
                Err(e) => Err(anyhow::anyhow!("indexer task died: {e}")),
            }
        }
        r = &mut api_task => {
            cancel.cancel();
            let _ = indexer_task.await;
            match r {
                Ok(Ok(())) => Err(anyhow::anyhow!("API task exited unexpectedly")),
                Ok(Err(e)) => Err(anyhow::anyhow!("API server failed: {e}")),
                Err(e) => Err(anyhow::anyhow!("API task died: {e}")),
            }
        }
    }
}
