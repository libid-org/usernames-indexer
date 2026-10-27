//! The indexing loop: read the head every poll interval and commit the
//! blocks it confirmed in `eth_getLogs` windows.
//!
//! Each window commits through [`crate::db::Window`] — journal rows, projection
//! writes and the cursor together — so a crash replays a whole window instead
//! of resuming inside one, and the journal's conflict gates every write, so
//! the replay converges.

mod deadline;

use std::time::Duration;

use alloy::{
    providers::{
        Provider,
        RootProvider,
    },
    rpc::{
        client::ClientBuilder,
        types::Log,
    },
    transports::{
        TransportErrorKind,
        TransportResult,
    },
};
use anyhow::Context;
use tokio_util::sync::CancellationToken;
use url::Url;

use self::deadline::Deadline;
use crate::{
    chain::Contracts,
    db::{
        ChainWriter,
        Superseded,
        WindowError,
    },
};

/// The loop's knobs, resolved by the CLI. The chain itself is carried by the
/// [`ChainWriter`] the [`Indexer`] is built over.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// The contracts to watch. The escrow's events share the registry's
    /// windows and cursor: it is deployed after the registry it names, so
    /// the registry's deployment block covers it.
    pub contracts: Contracts,
    /// Blocks behind the head this indexer stays. The loop never commits past
    /// `latest - confirmations`, so a reorg shallower than this cannot leave
    /// the read model holding events the canonical chain never emitted.
    pub confirmations: u64,
    /// The poll cycle, and the retry delay after a failure.
    pub poll_interval: Duration,
    /// Largest `eth_getLogs` window, sized to the RPC provider's limits.
    pub max_block_range: u64,
    /// Where a FRESH scan starts — used only when no cursor exists (a new
    /// database, or right after a re-index). When unset, the deployment block
    /// is detected by binary search over `eth_getCode` and cached.
    pub start_block: Option<u64>,
    /// How long readers may trust a report this loop makes. Set beside every
    /// target and renewed per committed chunk; a reader that finds it expired
    /// treats the index as stale. It has to cover a poll interval, a slow
    /// chunk and a missed cycle, which is why the CLI derives it from
    /// `poll_interval` when nothing sets it.
    pub stale_after: Duration,
}

impl IndexerConfig {
    /// A client for the `http(s)` endpoint `rpc` as this loop uses it, each
    /// call failing once unanswered for half of [`Self::stale_after`].
    pub fn connect(&self, rpc: &Url) -> TransportResult<RootProvider> {
        if !matches!(rpc.scheme(), "http" | "https") {
            return Err(TransportErrorKind::custom_str(&format!(
                "the RPC URL is {}, not http(s)",
                rpc.scheme()
            )));
        }
        let client = ClientBuilder::default()
            .layer(Deadline(self.patience()))
            .http(rpc.clone());
        Ok(RootProvider::new(client))
    }

    /// How long a call waits for an answer before it fails and the session
    /// ends: half the report's life, so a stuck endpoint is left and its
    /// error stored while the report still holds.
    fn patience(&self) -> Duration {
        self.stale_after / 2
    }

    /// The last block of the `eth_getLogs` window that starts at `from`, at
    /// most `through`.
    fn window_end(&self, from: u64, through: u64) -> u64 {
        from.saturating_add(self.max_block_range.max(1) - 1)
            .min(through)
    }
}

/// One chain's indexer: the chain's writer, the RPC endpoint and the knobs
/// fused, so the loop's operations take only what actually varies between
/// calls.
pub struct Indexer {
    writer: ChainWriter,
    rpc: Url,
    config: IndexerConfig,
}

impl Indexer {
    /// The indexer of `writer`'s chain over `rpc`.
    pub fn new(writer: ChainWriter, rpc: Url, config: IndexerConfig) -> Self {
        Self {
            writer,
            rpc,
            config,
        }
    }

    /// Apply `logs` — every watched log past the cursor through block
    /// `through`, in chain order — and commit them with the cursor. An error
    /// leaves the cursor where it was. Returns how many events were NEW —
    /// replayed journal rows are skipped by the window and not counted, so
    /// the caller's log line stays honest.
    async fn commit(&self, logs: &[Log], through: u64) -> anyhow::Result<usize> {
        let mut window = self.writer.begin_window().await?;
        let mut applied = 0usize;
        for log in logs {
            match self.config.contracts.decode(log) {
                Ok(Some((event, position))) => {
                    if window.apply(&event, &position).await? {
                        applied += 1;
                    }
                }
                Ok(None) => {
                    // A topic this build does not know. The contracts are
                    // upgradeable, so this is survivable — but it is not
                    // silent, because a new event type usually means the read
                    // model is due for a version bump.
                    tracing::warn!(
                        emitter = %log.address(),
                        topic0 = ?log.topic0(),
                        block = log.block_number,
                        "unknown event topic from a watched contract; skipped"
                    );
                }
                Err(e) => {
                    anyhow::bail!("undecodable log at block {:?}: {e}", log.block_number)
                }
            }
        }
        window.commit(through).await?;
        Ok(applied)
    }

    /// `error` with the RPC endpoint named by its setting. An HTTP client's
    /// error carries the URL it called, an endpoint's URL often holds its
    /// API key, and `/v1/status` shows the stored error to anyone.
    fn redacted(&self, error: String) -> String {
        let endpoint = &self.rpc.as_str()[self.rpc.scheme().len()..];
        error.replace(endpoint, "://<RPC_URL>")
    }

    /// Run until cancelled, or until the chain is no longer this writer's.
    /// Never returns early on an RPC or database error: those end the
    /// session, which logs, sleeps one interval, and opens another that
    /// resumes from the cursor.
    pub async fn run(self, cancel: CancellationToken) -> Result<(), Superseded> {
        tracing::info!(
            contract = %self.config.contracts.registry,
            escrow = ?self.config.contracts.escrow,
            chain_id = self.writer.store().chain_id(),
            confirmations = self.config.confirmations,
            "usernames indexer starting"
        );

        while !cancel.is_cancelled() {
            let session = async { Session::open(&self)?.follow(&cancel).await };
            if let Err(e) = session.await {
                if let Some(WindowError::Superseded(superseded)) = e.downcast_ref() {
                    return Err(*superseded);
                }
                let error = self.redacted(e.one_line());
                tracing::warn!(%error, "indexing stopped; resuming from the cursor");
                // Surfaced in /v1/status: a deterministic failure, an
                // undecodable log say, stalls the cursor rather than skip
                // the log, and a monitor has to see the stall.
                self.writer.set_window_error(Some(&error)).await;
                cancel
                    .run_until_cancelled(tokio::time::sleep(self.config.poll_interval))
                    .await;
            }
        }
        tracing::info!("usernames indexer shutting down");
        Ok(())
    }
}

/// An error and its causes as one line.
trait OneLine {
    /// Each cause once: a cause is left out when the line already ends with
    /// it as a whole part, as it does after an error that prints its cause
    /// in its own message and also returns it as its source.
    fn one_line(&self) -> String;
}

impl OneLine for anyhow::Error {
    fn one_line(&self) -> String {
        let mut line = String::new();
        for cause in self.chain() {
            let text = cause.to_string();
            if text.is_empty() || line == text || line.ends_with(&format!(": {text}")) {
                continue;
            }
            if !line.is_empty() {
                line.push_str(": ");
            }
            line.push_str(&text);
        }
        line
    }
}

/// One run of the loop over its own RPC client, from its first catch-up to
/// the cancellation or first error that ends it; the next session resumes
/// from the cursor.
struct Session<'a> {
    indexer: &'a Indexer,
    provider: RootProvider,
}

impl<'a> Session<'a> {
    /// A session over `indexer`'s endpoint.
    fn open(indexer: &'a Indexer) -> anyhow::Result<Self> {
        let provider = indexer
            .config
            .connect(&indexer.rpc)
            .context("connecting to the RPC")?;
        Ok(Self { indexer, provider })
    }

    /// Catch up now and again every poll interval, until cancelled.
    async fn follow(self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let interval = self.indexer.config.poll_interval;
        loop {
            self.catch_up(cancel).await?;
            if cancel
                .run_until_cancelled(tokio::time::sleep(interval))
                .await
                .is_none()
            {
                return Ok(());
            }
        }
    }

    /// Read the head and commit every confirmed block past the cursor in
    /// `eth_getLogs` windows. A head that has not passed the cursor's
    /// confirmation depth costs no `eth_getLogs` call.
    async fn catch_up(&self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let indexer = self.indexer;
        let config = &indexer.config;
        let (head, mut from) = tokio::try_join!(self.head(), self.next_block())?;
        let target = head.saturating_sub(config.confirmations);
        // Both, because they answer different questions: `head` is how far
        // the chain has grown, `target` is how far this loop intends to get.
        // A reader measuring staleness must use the second — the cursor is
        // never advanced past it.
        let ((), vouched) = tokio::join!(
            indexer.writer.set_chain_head(head),
            indexer.writer.set_chain_target(target, config.stale_after)
        );

        let mut total = 0usize;
        while from <= target && !cancel.is_cancelled() {
            let to = config.window_end(from, target);
            // Renewed before the fetch as well as after the commit: the fetch
            // is the slow part, and a report expiring during it would refuse
            // a loop that is busy catching up.
            if vouched {
                indexer.writer.touch_chain_target(config.stale_after).await;
            }
            total += self.commit_fetched(from, to).await?;
            from = to + 1;
        }
        indexer.writer.set_window_error(None).await;
        if total > 0 {
            tracing::info!(events = total, through_block = target, "indexed");
        }
        Ok(())
    }

    /// Fetch blocks `from..=to` and commit them with the cursor; how many
    /// events were new.
    async fn commit_fetched(&self, from: u64, to: u64) -> anyhow::Result<usize> {
        let logs = self.fetch(from, to).await?;
        self.indexer
            .commit(&logs, to)
            .await
            .with_context(|| format!("blocks {from}..{to}"))
    }

    /// The chain head, as the endpoint reports it.
    async fn head(&self) -> anyhow::Result<u64> {
        self.provider
            .get_block_number()
            .await
            .context("eth_blockNumber")
    }

    /// One `eth_getLogs` window, inclusive on both ends, in chain order.
    async fn fetch(&self, from: u64, to: u64) -> anyhow::Result<Vec<Log>> {
        let mut logs = self
            .indexer
            .config
            .contracts
            .logs(&self.provider, from, to)
            .await
            .with_context(|| format!("the logs of blocks {from}..{to}"))?;
        // Every log has to sit in the window the cursor will pass.
        anyhow::ensure!(
            logs.iter().all(|log| log
                .block_number
                .is_some_and(|block| (from..=to).contains(&block))),
            "eth_getLogs {from}..{to} answered a log outside the window"
        );
        logs.sort_by_key(|log| (log.block_number, log.log_index));
        Ok(logs)
    }

    /// The first block past the cursor, or where a fresh scan starts.
    async fn next_block(&self) -> anyhow::Result<u64> {
        match self
            .indexer
            .writer
            .store()
            .cursor()
            .await
            .context("reading the cursor")?
        {
            Some(cursor) => Ok(cursor + 1),
            None => self
                .resolve_start_block()
                .await
                .context("no start block: deployment detection failed on RPC"),
        }
    }

    /// Where a fresh scan starts: the override, the cached detection, or a
    /// new binary search. `None` means the RPC failed mid-detection — the
    /// session ends and the next one retries instead of committing to a
    /// guess. Only a genuine detection is cached: a cached fallback would
    /// outlive the failure that produced it and pin this deployment to
    /// genesis scans forever.
    async fn resolve_start_block(&self) -> Option<u64> {
        let indexer = self.indexer;
        let contracts = indexer.config.contracts;
        if let Some(block) = indexer.config.start_block {
            return Some(block);
        }
        match indexer
            .writer
            .store()
            .deploy_block(contracts.registry)
            .await
        {
            Ok(Some(block)) => return Some(block),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "failed to read the cached deployment block")
            }
        }
        let latest = match self.provider.get_block_number().await {
            Ok(n) => n,
            Err(error) => {
                tracing::warn!(%error, "no latest block for deployment detection");
                return None;
            }
        };
        match contracts
            .registry_deployment_block(&self.provider, latest)
            .await
        {
            Ok(Some(block)) => {
                tracing::info!(block, "detected contract deployment block");
                if let Err(error) = indexer.writer.set_deploy_block(block).await {
                    tracing::warn!(%error, "failed to cache the deployment block");
                }
                Some(block)
            }
            Ok(None) => {
                // Absence is an answer, but not one worth caching: the
                // contract may simply not be deployed yet. Scanning from
                // genesis is slow but never wrong, and the next fresh cycle
                // re-checks.
                tracing::warn!(
                    "the contract has no code at the chain head; scanning from genesis"
                );
                Some(0)
            }
            Err(error) => {
                tracing::warn!(%error, "deployment detection failed on RPC");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;
    use sqlx::PgPool;
    use tokio::net::TcpListener;

    use super::*;
    use crate::db::ChainStore;

    /// The suite's knobs, a report good for `stale_after`.
    fn config(stale_after: Duration) -> IndexerConfig {
        IndexerConfig {
            contracts: Contracts {
                registry: Address::ZERO,
                escrow: None,
            },
            confirmations: 3,
            poll_interval: Duration::from_secs(5),
            max_block_range: 10,
            start_block: None,
            stale_after,
        }
    }

    /// An endpoint that takes the connection and never answers fails each
    /// call once the patience runs out.
    #[tokio::test]
    async fn a_call_left_unanswered_fails_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let rpc: Url = format!("http://{}", listener.local_addr().expect("the address"))
            .parse()
            .expect("a URL");
        let holding = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let provider = config(Duration::from_millis(400))
            .connect(&rpc)
            .expect("an http client");
        let answered =
            tokio::time::timeout(Duration::from_secs(10), provider.get_block_number())
                .await
                .expect("the call gave up at its deadline");
        let Err(error) = answered else {
            panic!("an endpoint that never answered answered");
        };
        assert!(error.to_string().contains("no answer within"), "{error}");
        holding.abort();
    }

    #[test]
    fn an_rpc_url_other_than_http_is_refused() {
        let config = config(Duration::from_secs(80));
        for rpc in ["wss://node.example/rpc", "file:///tmp/geth.ipc"] {
            let rpc: Url = rpc.parse().expect("a URL");
            assert!(config.connect(&rpc).is_err(), "{rpc}");
        }
        let http: Url = "https://node.example/rpc".parse().expect("a URL");
        assert!(config.connect(&http).is_ok());
    }

    #[tokio::test]
    async fn a_stored_error_names_the_rpc_url_by_its_setting() {
        let pool = PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let indexer = Indexer::new(
            ChainWriter::unprepared(ChainStore::new(pool, 1)),
            "https://eth-mainnet.g.alchemy.com/v2/secret"
                .parse()
                .unwrap(),
            config(Duration::from_secs(80)),
        );
        let error = "eth_blockNumber: error sending request for url \
                     (https://eth-mainnet.g.alchemy.com/v2/secret)";
        let stored = indexer.redacted(error.to_string());
        assert!(!stored.contains("secret"), "{stored}");
        assert!(stored.contains("<RPC_URL>"), "{stored}");
    }

    /// An error that prints its cause in its own message and returns it as
    /// its source, as sqlx's and alloy's do.
    #[derive(Debug, thiserror::Error)]
    #[error("answer lost: {0}")]
    struct Repeats(#[source] std::io::Error);

    #[test]
    fn a_stored_error_names_each_cause_once() {
        let error = anyhow::Error::new(Repeats(std::io::Error::other("socket closed")))
            .context("eth_blockNumber");
        assert_eq!(
            error.one_line(),
            "eth_blockNumber: answer lost: socket closed"
        );
    }
}
