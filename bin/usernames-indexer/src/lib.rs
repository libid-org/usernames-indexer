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
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// JSON-RPC endpoint of the chain to follow.
    #[arg(long, env = "RPC_URL", hide_env_values = true)]
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

    /// How long readers may trust this indexer's last report, in seconds. The
    /// API refuses a chain whose report has expired, so this decides how soon
    /// a stopped indexer is noticed — and how long a slow chunk or a missed
    /// cycle may take without a healthy loop reading as stopped. Unset means
    /// four poll intervals plus a minute.
    #[arg(long, env = "STALE_AFTER_SECS")]
    pub stale_after_secs: Option<u64>,
}

/// The longest a report may be trusted: a year. Past that the setting is a
/// mistake, and the store would clamp it anyway.
const MAX_STALE_AFTER_SECS: u64 = 366 * 24 * 60 * 60;

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
    // Pure configuration, checked before anything is touched: `prepare`
    // below may wipe the chain's rows on a version bump, and a refusal has
    // to come before that, not after.
    let stale_after_secs = config.stale_after_secs.unwrap_or(
        config
            .poll_interval_secs
            .saturating_mul(4)
            .saturating_add(60),
    );
    anyhow::ensure!(
        stale_after_secs > config.poll_interval_secs,
        "STALE_AFTER_SECS ({stale_after_secs}) must exceed POLL_INTERVAL_SECS ({}), or \
         every idle cycle expires the report before the next one renews it",
        config.poll_interval_secs
    );
    anyhow::ensure!(
        stale_after_secs <= MAX_STALE_AFTER_SECS,
        "STALE_AFTER_SECS ({stale_after_secs}) is more than a year; a report nobody \
         expects to expire is not a report"
    );

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
            stale_after_secs,
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
        for secret in ["database_url", "rpc_url"] {
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
