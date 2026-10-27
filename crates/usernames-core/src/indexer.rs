//! The indexing loop: catch up with `eth_getLogs` windows, then follow the
//! chain, by polling or from pushed heads.
//!
//! Each window commits through [`crate::db::Window`] — journal rows, projection
//! writes and the cursor together — so a crash replays a whole window instead
//! of resuming inside one, and the journal's conflict gates every write, so
//! the replay converges.

mod heads;
mod source;

use std::time::Duration;

use tokio::time::Instant;

use alloy::{
    providers::{
        Provider,
        RootProvider,
    },
    rpc::types::Log,
};
use anyhow::Context;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::source::{
    LogSource,
    UnknownLogSource,
};
use self::{
    heads::Heads,
    source::{
        Feed,
        Wake,
    },
};
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
    /// the read model holding events the canonical chain never emitted. At
    /// least one with a subscription, whose logs for the head block may still
    /// be on their way.
    pub confirmations: u64,
    /// How the loop learns of new blocks.
    pub source: LogSource,
    /// The retry delay after a failure, and with [`LogSource::Poll`] the poll
    /// cycle.
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
    /// `poll_interval` when nothing sets it. A subscription that hears no
    /// head for half of it reconnects.
    pub stale_after: Duration,
}

impl IndexerConfig {
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
    /// API key, and `/v1/status` shows the stored error to anyone. Matched
    /// past the scheme, so the WebSocket twin is caught too.
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
            contracts = ?self.config.contracts,
            chain_id = self.writer.store().chain_id(),
            confirmations = self.config.confirmations,
            source = %self.config.source,
            "usernames indexer starting"
        );

        while !cancel.is_cancelled() {
            let session = async { Session::open(&self).await?.follow(&cancel).await };
            match session.await {
                Ok(Ended::Cancelled) => {}
                Ok(Ended::Quiet { silent, grown: 0 }) => {
                    tracing::info!(?silent, "no head pushed; reconnecting");
                }
                Ok(Ended::Quiet { silent, grown }) => {
                    tracing::warn!(
                        ?silent,
                        grown,
                        "the chain grew while the endpoint pushed no head; reconnecting"
                    );
                }
                Err(e) => {
                    if let Some(WindowError::Superseded(superseded)) = e.downcast_ref() {
                        return Err(*superseded);
                    }
                    let error = self.redacted(format!("{e:#}"));
                    tracing::warn!(%error, "indexing stopped; resuming from the cursor");
                    // Surfaced in /v1/status: a deterministic failure (an
                    // undecodable log, say) stalls the cursor by design —
                    // integrity over progress — and a stall must be visible
                    // somewhere a monitor actually looks.
                    self.writer.set_window_error(Some(&error)).await;
                    cancel
                        .run_until_cancelled(tokio::time::sleep(
                            self.config.poll_interval,
                        ))
                        .await;
                }
            }
        }
        tracing::info!("usernames indexer shutting down");
        Ok(())
    }
}

/// How a session that did not fail ended.
enum Ended {
    /// The indexer was cancelled.
    Cancelled,
    /// No head was pushed for half the report's life: the next session
    /// reconnects and catches up.
    Quiet {
        /// How long the session heard no head.
        silent: Duration,
        /// Blocks the endpoint's head grew past the last head the session
        /// saw: some when the endpoint stopped pushing, none on an idle chain
        /// or when the endpoint's node stopped following the chain.
        grown: u64,
    },
}

/// One connection to the RPC, from the catch-up that opens it to the
/// cancellation, quiet stretch or first error that ends it. Whatever a
/// dropped socket or an overflowing stream skipped, the next session's
/// catch-up fetches.
struct Session<'a> {
    indexer: &'a Indexer,
    provider: RootProvider,
    feed: Feed,
}

impl<'a> Session<'a> {
    /// Connect, and open what the configured source follows the chain with.
    async fn open(indexer: &'a Indexer) -> anyhow::Result<Self> {
        let source = indexer.config.source;
        let provider = source
            .connect(&indexer.rpc)
            .await
            .context("connecting to the RPC")?;
        let feed = Feed::open(source, &provider)
            .await
            .context("eth_subscribe")?;
        Ok(Self {
            indexer,
            provider,
            feed,
        })
    }

    /// Catch up, then follow: a poll catches up again every interval; a
    /// subscription commits through each pushed head's confirmation depth,
    /// fetching only the blocks its heads cannot vouch are free of watched
    /// logs. A subscription that hears no head for half the report's life
    /// ends the session, so the next one reconnects and catches up: an open
    /// but silent socket would otherwise leave the index frozen and the
    /// report never renewed.
    async fn follow(mut self, cancel: &CancellationToken) -> anyhow::Result<Ended> {
        let config = &self.indexer.config;
        let watched = config.contracts.watched();
        let quiet = config.stale_after / 2;
        let mut seen = self.catch_up(cancel).await?;
        let mut heads = Heads::default();
        let mut heard = Instant::now();
        loop {
            let until = match self.feed {
                Feed::Poll => Instant::now() + config.poll_interval,
                Feed::Subscribe(_) => heard + quiet,
            };
            match self.feed.wait(until, cancel).await? {
                Wake::Cancelled => return Ok(Ended::Cancelled),
                Wake::Tick => match self.feed {
                    Feed::Poll => {
                        self.catch_up(cancel).await?;
                    }
                    Feed::Subscribe(_) => {
                        let silent = heard.elapsed();
                        let grown = self.head().await?.saturating_sub(seen);
                        return Ok(Ended::Quiet { silent, grown });
                    }
                },
                Wake::Head(head) => {
                    heard = Instant::now();
                    seen = head.number;
                    heads.push(&head, &watched);
                    self.advance(head.number, &mut heads).await?;
                }
            }
        }
    }

    /// Read the head and commit every confirmed block past the cursor in
    /// `eth_getLogs` windows; the head it read.
    async fn catch_up(&mut self, cancel: &CancellationToken) -> anyhow::Result<u64> {
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
        Ok(head)
    }

    /// Commit through `head`'s confirmation depth: fetched when the heads
    /// say a block there may hold a watched log, passed without a call
    /// otherwise.
    async fn advance(&mut self, head: u64, heads: &mut Heads) -> anyhow::Result<()> {
        let indexer = self.indexer;
        let config = &indexer.config;
        let target = head.saturating_sub(config.confirmations);
        let from = self.next_block().await?;
        if from <= target {
            if heads.settle(from, target) {
                let mut start = from;
                let mut total = 0usize;
                while start <= target {
                    let to = config.window_end(start, target);
                    total += self.commit_fetched(start, to).await?;
                    start = to + 1;
                }
                if total > 0 {
                    tracing::info!(events = total, through_block = target, "indexed");
                }
            } else {
                indexer
                    .commit(&[], target)
                    .await
                    .with_context(|| format!("blocks {from}..{target}"))?;
            }
        }
        tokio::join!(
            indexer.writer.set_chain_head(head),
            indexer.writer.set_chain_target(target, config.stale_after),
            indexer.writer.set_window_error(None),
        );
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
        let mut logs = (self.indexer.config.contracts)
            .logs(&self.provider, from, to)
            .await
            .with_context(|| format!("eth_getLogs {from}..{to}"))?;
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
            Err(e) => tracing::warn!(%e, "failed to read the cached deployment block"),
        }
        let latest = match self.provider.get_block_number().await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(%e, "no latest block for deployment detection");
                return None;
            }
        };
        match contracts.deployment_block(&self.provider, latest).await {
            Ok(Some(block)) => {
                tracing::info!(block, "detected contract deployment block");
                if let Err(e) = indexer.writer.set_deploy_block(block).await {
                    tracing::warn!(%e, "failed to cache the deployment block");
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
            Err(e) => {
                tracing::warn!(%e, "deployment detection failed on RPC");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;
    use sqlx::PgPool;

    use super::*;
    use crate::db::ChainStore;

    #[tokio::test]
    async fn a_stored_error_names_the_rpc_url_by_its_setting() {
        let pool = PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let indexer = Indexer::new(
            ChainWriter::unprepared(ChainStore::new(pool, 1)),
            "https://eth-mainnet.g.alchemy.com/v2/secret"
                .parse()
                .unwrap(),
            IndexerConfig {
                contracts: Contracts {
                    registry: Address::ZERO,
                    escrow: None,
                },
                confirmations: 3,
                source: LogSource::Subscribe,
                poll_interval: Duration::from_secs(5),
                max_block_range: 10,
                start_block: None,
                stale_after: Duration::from_secs(80),
            },
        );
        for called in [
            "https://eth-mainnet.g.alchemy.com/v2/secret",
            "wss://eth-mainnet.g.alchemy.com/v2/secret",
        ] {
            let error =
                format!("eth_blockNumber: error sending request for url ({called})");
            let stored = indexer.redacted(error);
            assert!(!stored.contains("secret"), "{stored}");
            assert!(stored.contains("<RPC_URL>"), "{stored}");
        }
    }
}
