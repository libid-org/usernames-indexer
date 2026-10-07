//! The indexer: one process per chain, mirroring `IdentityRegistry` and
//! `HandleEscrow` events into Postgres under that chain's writer lease.
//!
//! It exposes no port. An indexer that stops advancing exits, and the
//! supervisor notices; the read API is a separate binary over the same
//! database.

#![deny(missing_docs)]
#![deny(dead_code)]

use std::time::Duration;

use alloy::{
    primitives::Address,
    providers::{
        Provider,
        RootProvider,
    },
};
use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use url::Url;
use usernames_core::{
    chain,
    db,
    ens::ChainName,
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

    /// The IdentityRegistry ERC1967 proxy address. Typed as an address so
    /// garbage is a usage error before anything connects, not a mid-startup
    /// failure.
    #[arg(long, env = "IDENTITY_NAMES_ADDRESS")]
    pub identity_names_address: Address,

    /// The HandleEscrow ERC1967 proxy, when the chain has one. Its `registry`
    /// must be `IDENTITY_NAMES_ADDRESS`: the escrow pays whoever that registry
    /// says holds a handle, and the index joins the two on that. Setting it,
    /// changing it or unsetting it replays the chain.
    #[arg(long, env = "HANDLE_ESCROW_ADDRESS")]
    pub handle_escrow_address: Option<Address>,

    /// Refuse to start unless the RPC reports this chain id. Optional, but a
    /// deployment that sets it cannot silently index the wrong chain.
    #[arg(long, env = "CHAIN_ID")]
    pub chain_id: Option<u64>,

    /// The names this chain goes by in an ENS name, comma-separated: the
    /// `base` in `alice.x.base.handles.link`. Labels only — lowercase ASCII
    /// letters, digits and hyphens — and never a platform key. Written to the
    /// store at every start for the gateway to read, replacing what this
    /// chain declared before; a name belongs to one chain across the store.
    #[arg(long, env = "CHAIN_NAMES", value_delimiter = ',', required = true)]
    pub chain_names: Vec<ChainName>,

    /// Blocks behind the head to stay; shallow-reorg protection.
    #[arg(long, env = "CONFIRMATIONS", default_value_t = 5)]
    pub confirmations: u64,

    /// Time between poll cycles, and the retry delay after a failure:
    /// `5s`, `1500ms`.
    #[arg(long, env = "POLL_INTERVAL", default_value = "5s", value_parser = humantime::parse_duration)]
    pub poll_interval: Duration,

    /// Largest eth_getLogs window, sized to the RPC provider's limits.
    #[arg(long, env = "MAX_BLOCK_RANGE", default_value_t = 10_000)]
    pub max_block_range: u64,

    /// Scan start override. Unset means: detect the deployment block.
    #[arg(long, env = "START_BLOCK")]
    pub start_block: Option<u64>,

    /// How long readers may trust this indexer's last report: `80s`, `2m`.
    /// The API refuses a chain whose report has expired, so this decides how
    /// soon a stopped indexer is noticed — and how long a slow chunk or a
    /// missed cycle may take without a healthy loop reading as stopped. Unset
    /// means four poll intervals plus a minute.
    #[arg(long, env = "STALE_AFTER", value_parser = humantime::parse_duration)]
    pub stale_after: Option<Duration>,
}

/// The longest a report may be trusted: a year. Past that the setting is a
/// mistake, and the store would clamp it anyway.
const MAX_STALE_AFTER: Duration = Duration::from_secs(366 * 24 * 60 * 60);

impl Config {
    /// How long readers may trust a report: `STALE_AFTER`, or four poll
    /// intervals plus a minute. Refused unless it outlasts a poll interval,
    /// or every idle cycle would expire the report before the next one
    /// renews it, and unless it stays within a year.
    fn report_validity(&self) -> anyhow::Result<Duration> {
        let validity = self.stale_after.unwrap_or(
            self.poll_interval
                .saturating_mul(4)
                .saturating_add(Duration::from_secs(60)),
        );
        anyhow::ensure!(
            validity > self.poll_interval,
            "STALE_AFTER ({}) must exceed POLL_INTERVAL ({}), or every idle cycle \
             expires the report before the next one renews it",
            humantime::format_duration(validity),
            humantime::format_duration(self.poll_interval)
        );
        anyhow::ensure!(
            validity <= MAX_STALE_AFTER,
            "STALE_AFTER ({}) is more than a year; a report nobody expects to \
             expire is not a report",
            humantime::format_duration(validity)
        );
        Ok(validity)
    }
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
    // Pure configuration, checked before anything is touched: `prepare`
    // below may wipe the chain's rows on a version bump, and a refusal has
    // to come before that, not after.
    let stale_after = config.report_validity()?;

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
    let escrow = config.handle_escrow_address;
    if let Some(escrow) = escrow {
        // Before `prepare`, which clears the chain when the escrow changes: a
        // misconfiguration refuses without touching the rows.
        chain::check_escrow(&provider, escrow, contract)
            .await
            .context("HANDLE_ESCROW_ADDRESS")?;
    }

    // The indexer migrates because the indexer writes. The read API connects
    // to whatever schema it finds.
    let pool = db::connect_and_migrate(&config.database_url).await?;
    let store = db::ChainStore::new(pool, chain_id);
    // The writer lease outlives everything below: as long as this process
    // may write the chain, no other instance may. prepare runs under it —
    // the lease is a parameter there, so a version-bump replay cannot race a
    // still-running older pod.
    let writer = store.acquire_writer().await?;
    store.prepare(&writer, contract, escrow).await?;
    store.set_chain_names(&writer, &config.chain_names).await?;

    let cancel = CancellationToken::new();
    let indexer = indexer::Indexer::new(
        store,
        provider,
        indexer::IndexerConfig {
            contract,
            escrow,
            confirmations: config.confirmations,
            poll_interval: config.poll_interval,
            max_block_range: config.max_block_range,
            start_block: config.start_block,
            stale_after,
        },
    );
    tracing::info!(chain_id, %contract, ?escrow, "indexing");
    let mut task = tokio::spawn(indexer.run(cancel.clone()));

    tokio::select! {
        r = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
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
    use clap::{
        CommandFactory,
        Parser,
    };

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

    /// The chain names are parsed by clap into their type, so a platform key
    /// or a malformed label stops the process before anything connects.
    #[test]
    fn chain_names_are_refused_at_parse() {
        let args = |names: &str| {
            Config::try_parse_from([
                "usernames-indexer",
                "--database-url",
                "postgres://localhost/usernames",
                "--rpc-url",
                "http://127.0.0.1:8545",
                "--identity-names-address",
                "0x0000000000000000000000000000000000000001",
                "--chain-names",
                names,
            ])
        };
        let names: Vec<String> = args("eden,base")
            .expect("two labels")
            .chain_names
            .iter()
            .map(|name| name.as_str().to_string())
            .collect();
        assert_eq!(names, ["eden", "base"]);
        for bad in ["eden,x", "Eden", "eden,"] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }
}
