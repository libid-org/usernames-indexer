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
    providers::Provider,
};
use anyhow::Context;
use clap::Parser;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;
use usernames_core::{
    chain::Contracts,
    db::{
        self,
        Superseded,
        WriterLease,
    },
    ens::ChainName,
    indexer::{
        self,
        LogSource,
    },
    stop::StopSignals,
};

/// Everything comes from flags or the environment; a `.env` file is read
/// first so local runs need no exports.
#[derive(Debug, Parser)]
#[command(name = "usernames-indexer", about)]
pub struct Config {
    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// JSON-RPC endpoint of the chain to follow. With the `subscribe` log
    /// source an `http(s)` URL is dialled as `ws(s)` on the same host and
    /// path, which suits Alchemy; a node or provider whose WebSocket sits on
    /// another port or path takes a `ws(s)` URL here.
    #[arg(long, env = "RPC_URL", hide_env_values = true)]
    pub rpc_url: Url,

    /// How the indexer learns of new blocks: `subscribe` holds an
    /// `eth_subscribe("newHeads")` stream on a WebSocket; `poll` reads the
    /// head and asks with `eth_getLogs` every `POLL_INTERVAL`, for an
    /// endpoint that serves no WebSocket.
    #[arg(long, env = "LOG_SOURCE", default_value = "subscribe")]
    pub log_source: LogSource,

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

    /// Blocks behind the head to stay; shallow-reorg protection. At least one
    /// with the `subscribe` log source.
    #[arg(long, env = "CONFIRMATIONS", default_value_t = 5)]
    pub confirmations: u64,

    /// The retry delay after a failure, and with `LOG_SOURCE=poll` the poll
    /// cycle: `5s`, `1500ms`.
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

impl Config {
    /// The contracts this indexer follows.
    fn contracts(&self) -> Contracts {
        Contracts {
            registry: self.identity_names_address,
            escrow: self.handle_escrow_address,
        }
    }

    /// The indexer this configuration describes, under its chain's writer
    /// lease: endpoints checked, the schema migrated, the chain prepared and
    /// its names declared.
    async fn start(self) -> anyhow::Result<(WriterLease, indexer::Indexer)> {
        let contracts = self.contracts();
        // Pure configuration, checked before anything is touched: `prepare`
        // below may wipe the chain's rows on a version bump, and a refusal has
        // to come before that, not after.
        let stale_after = self.report_validity()?;
        let confirmations = self.confirmations()?;

        // The same transport the loop will use, so an endpoint that serves no
        // WebSocket refuses here, at startup, instead of in every session.
        let provider = self.log_source.connect(&self.rpc_url).await.map_err(|e| {
            anyhow::anyhow!("RPC_URL over LOG_SOURCE={}: {e}", self.log_source)
        })?;
        let reported = provider.get_chain_id().await?;
        if let Some(expected) = self.chain_id {
            anyhow::ensure!(
                reported == expected,
                "RPC reports chain id {reported}, config expects {expected}"
            );
        }
        let chain_id = i64::try_from(reported).map_err(|_| {
            anyhow::anyhow!("chain id {reported} does not fit in a BIGINT")
        })?;
        // Before `prepare` too: an older registry's events would be misread.
        contracts
            .check_registry(&provider)
            .await
            .context("IDENTITY_NAMES_ADDRESS")?;
        // Before `prepare`, which clears the chain when the escrow changes: a
        // misconfiguration refuses without touching the rows.
        contracts
            .check_escrow(&provider)
            .await
            .context("HANDLE_ESCROW_ADDRESS")?;

        // The indexer migrates because the indexer writes. The read API connects
        // to whatever schema it finds.
        let pool = db::connect_and_migrate(&self.database_url).await?;
        let store = db::ChainStore::new(pool, chain_id);
        // The writer lease outlives the indexer: as long as this process may
        // write the chain, no other instance may. prepare runs under it —
        // the lease is a parameter there, so a version-bump replay cannot race a
        // still-running older pod.
        let lease = store.acquire_writer().await?;
        let writer = store.prepare(&lease, contracts).await?;
        store.set_chain_names(&lease, &self.chain_names).await?;

        // Startup's own connection: every session of the loop opens its own.
        drop(provider);
        let indexer = indexer::Indexer::new(
            writer,
            self.rpc_url,
            indexer::IndexerConfig {
                contracts,
                confirmations,
                source: self.log_source,
                poll_interval: self.poll_interval,
                max_block_range: self.max_block_range,
                start_block: self.start_block,
                stale_after,
            },
        );
        tracing::info!(
            chain_id,
            contract = %contracts.registry,
            escrow = ?contracts.escrow,
            "indexing"
        );
        Ok((lease, indexer))
    }

    /// How many blocks behind the head to stay: `CONFIRMATIONS`, refused at
    /// zero with a subscription. An endpoint can push a head before it
    /// serves that block's logs, and fetching them then would find none.
    fn confirmations(&self) -> anyhow::Result<u64> {
        anyhow::ensure!(
            self.confirmations > 0 || self.log_source == LogSource::Poll,
            "LOG_SOURCE=subscribe needs CONFIRMATIONS of at least 1: an endpoint \
             can push a head before it serves that block's logs"
        );
        Ok(self.confirmations)
    }

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
            validity <= db::MAX_VALID_FOR,
            "STALE_AFTER ({}) is more than a year; a report nobody expects to \
             expire is not a report",
            humantime::format_duration(validity)
        );
        Ok(validity)
    }
}

/// How often the indexer asks its writer lease's session whether it still
/// answers, and how long the session has to answer.
const LEASE_CHECK: Duration = Duration::from_secs(5);

/// Parse the environment, connect everything, and index until ctrl-c or
/// SIGTERM.
pub async fn run() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::parse();
    // From the start, so a stop request during a long startup (waiting on
    // another instance's lease, say) ends the process.
    let mut signals = StopSignals::listen()?;
    let (mut lease, indexer) = tokio::select! {
        started = config.start() => started?,
        () = signals.received() => {
            tracing::info!("stopped before indexing");
            return Ok(());
        }
    };
    let mut indexing = Indexing::start(indexer);

    let stopped = tokio::select! {
        () = signals.received() => {
            tracing::info!("shutting down");
            indexing.stop(LEASE_CHECK).await;
            Ok(())
        }
        r = &mut indexing.task => match r {
            Ok(Ok(())) => Err(anyhow::anyhow!("indexer task exited unexpectedly")),
            // The lease's session is most likely gone, and releasing a dead
            // one fails or hangs; dropping the lease closes it either way.
            Ok(Err(superseded)) => return Err(superseded.into()),
            Err(e) => Err(anyhow::anyhow!("indexer task died: {e}")),
        },
        // Another instance may take the chain once the lock is gone, so this
        // one stops writing and exits for its supervisor to restart.
        lost = lease.lost(LEASE_CHECK) => {
            indexing.stop(LEASE_CHECK).await;
            return Err(anyhow::Error::new(lost).context("the writer lease was lost"));
        }
    };
    // The loop no longer writes: the chain is the next instance's. Bounded
    // like the stop, since the database may be the reason it stopped;
    // dropping the lease closes its session either way.
    match tokio::time::timeout(LEASE_CHECK, lease.release()).await {
        Ok(released) => released?,
        Err(_) => {
            tracing::warn!(within = ?LEASE_CHECK, "the writer lease was not released in time")
        }
    }
    stopped
}

/// The indexer loop running as a task, and the token that stops it.
struct Indexing {
    cancel: CancellationToken,
    task: JoinHandle<Result<(), Superseded>>,
}

impl Indexing {
    /// Run `indexer` until it is stopped.
    fn start(indexer: indexer::Indexer) -> Self {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(indexer.run(cancel.clone()));
        Self { cancel, task }
    }

    /// Cancel the loop and give it `within` to finish its window. A loop
    /// stuck in a write to a silent database is aborted: its transaction
    /// commits whole or not at all, before any later writer's prepare, and
    /// the writer generation keeps anything after from landing.
    async fn stop(&mut self, within: Duration) {
        self.cancel.cancel();
        if tokio::time::timeout(within, &mut self.task).await.is_err() {
            tracing::warn!(?within, "the indexer did not stop in time; aborting it");
            self.task.abort();
        }
    }
}

#[cfg(test)]
mod help_tests {
    use clap::{
        CommandFactory,
        Parser,
    };

    use super::{
        Config,
        LogSource,
    };

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

    /// Without a setting the indexer subscribes; `poll` is the opt-out, and
    /// any other name stops the process before anything connects.
    #[test]
    fn the_log_source_defaults_to_a_subscription() {
        let args = |extra: &[&str]| {
            let required = [
                "usernames-indexer",
                "--database-url",
                "postgres://localhost/usernames",
                "--rpc-url",
                "http://127.0.0.1:8545",
                "--identity-names-address",
                "0x0000000000000000000000000000000000000001",
                "--chain-names",
                "eden",
            ];
            Config::try_parse_from(required.iter().chain(extra))
        };
        assert_eq!(
            args(&[]).expect("defaults").log_source,
            LogSource::Subscribe
        );
        assert_eq!(
            args(&["--log-source", "poll"]).expect("poll").log_source,
            LogSource::Poll
        );
        assert!(args(&["--log-source", "websocket"]).is_err());
    }

    /// A subscription needs a confirmation; a poll may run at the head.
    #[test]
    fn a_subscription_refuses_zero_confirmations() {
        let args = |extra: &[&str]| {
            let required = [
                "usernames-indexer",
                "--database-url",
                "postgres://localhost/usernames",
                "--rpc-url",
                "http://127.0.0.1:8545",
                "--identity-names-address",
                "0x0000000000000000000000000000000000000001",
                "--chain-names",
                "eden",
                "--confirmations",
                "0",
            ];
            Config::try_parse_from(required.iter().chain(extra)).expect("parses")
        };
        assert!(args(&[]).confirmations().is_err());
        assert_eq!(
            args(&["--log-source", "poll"]).confirmations().ok(),
            Some(0)
        );
    }
}

#[cfg(test)]
mod indexing_tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::Indexing;

    /// A loop that never reaches its next cancellation check is aborted
    /// once its time is up, so the process can exit.
    #[tokio::test]
    async fn a_loop_that_does_not_stop_is_aborted() {
        let mut indexing = Indexing {
            cancel: CancellationToken::new(),
            task: tokio::spawn(std::future::pending()),
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            indexing.stop(Duration::from_millis(50)),
        )
        .await
        .expect("stop returns");
        let ended = tokio::time::timeout(Duration::from_secs(1), indexing.task)
            .await
            .expect("the loop ended");
        assert!(ended.unwrap_err().is_cancelled());
    }
}
