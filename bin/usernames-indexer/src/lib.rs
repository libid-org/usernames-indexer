//! The write half: mirror `IdentityNames` events into Postgres, and serve
//! nothing.
//!
//! One process per chain, holding that chain's writer lease for as long as it
//! lives. The read API is a separate binary over the same database; nothing
//! here listens on a socket, so an indexer that stops advancing cannot hide
//! behind a healthy-looking endpoint — it exits, and the supervisor notices.

#![deny(missing_docs)]

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
use usernames_core::{
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
}

/// Parse the environment, connect everything, and index until ctrl-c.
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

    // The indexer migrates because the indexer writes. The read API connects
    // to whatever schema it finds.
    let pool = db::connect_and_migrate(&config.database_url).await?;
    let store = db::ChainStore::new(pool, chain_id);
    // The writer lease outlives everything below: as long as this process
    // may write the chain, no other instance may. prepare runs under it —
    // the lease is a parameter there, so a version-bump replay cannot race a
    // still-running older pod.
    let writer = store.acquire_writer().await?;
    store.prepare(&writer, contract).await?;

    let cancel = CancellationToken::new();
    let indexer = indexer::Indexer::new(
        store,
        provider,
        indexer::IndexerConfig {
            contract,
            confirmations: config.confirmations,
            poll_interval_secs: config.poll_interval_secs,
            max_block_range: config.max_block_range,
            start_block: config.start_block,
        },
    );
    info!(chain_id, %contract, "indexing");
    let mut task = tokio::spawn(indexer.run(cancel.clone()));

    tokio::select! {
        r = tokio::signal::ctrl_c() => {
            info!("shutting down");
            cancel.cancel();
            let _ = task.await;
            r?;
            Ok(())
        }
        r = &mut task => match r {
            Ok(()) => Err(anyhow::anyhow!("indexer task exited unexpectedly")),
            Err(e) => Err(anyhow::anyhow!("indexer task died: {e}")),
        },
    }
}
