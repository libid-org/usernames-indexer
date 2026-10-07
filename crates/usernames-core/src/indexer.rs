//! The indexing loop: catch up with `eth_getLogs` windows, then follow the
//! chain, by polling or from a log subscription.
//!
//! Each window commits through [`crate::db::Window`] — journal rows, projection
//! writes and the cursor together — so a crash replays a whole window instead
//! of resuming inside one, and every write in the window is an idempotent
//! upsert so the replay converges.

mod source;
mod unconfirmed;

use std::time::Duration;

use alloy::{
    eips::BlockNumberOrTag,
    primitives::Address,
    providers::{
        Provider,
        RootProvider,
    },
    rpc::types::{
        Filter,
        Log,
    },
};
use anyhow::Context;
use futures::future::try_join_all;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use self::source::{
    LogSource,
    UnknownLogSource,
};
use self::{
    source::Feed,
    unconfirmed::Forks,
};
use crate::{
    chain,
    db::ChainStore,
    events::{
        self,
        DecodeError,
        LogPosition,
        NamesEvent,
    },
};

/// The loop's knobs, resolved by the CLI. The chain itself is carried by the
/// [`ChainStore`] the [`Indexer`] is built over.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// The IdentityRegistry ERC1967 proxy — the address to watch. The
    /// implementation behind it changes on upgrade; the proxy is the one that
    /// emits.
    pub contract: Address,
    /// The HandleEscrow ERC1967 proxy that resolves through `contract`, when
    /// the chain has one. Its events share the registry's windows and cursor:
    /// it is deployed after the registry it names, so the registry's
    /// deployment block covers it.
    pub escrow: Option<Address>,
    /// Blocks behind the head this indexer stays. The loop never commits past
    /// `latest - confirmations`, so a reorg shallower than this cannot leave
    /// the read model holding events the canonical chain never emitted. At
    /// least one with a subscription, whose logs for the head block may still
    /// be on their way.
    pub confirmations: u64,
    /// How the loop learns of new logs.
    pub source: LogSource,
    /// The loop's cadence: a poll cycle, or with a subscription a renewal of
    /// the report and, while pushed logs wait for their confirmations, a
    /// head read. Also the retry delay after a failure.
    pub poll_interval: Duration,
    /// With a subscription, how often an idle loop reads the head to carry
    /// the cursor past blocks that hold no event.
    pub head_interval: Duration,
    /// Largest `eth_getLogs` window, sized to the RPC provider's limits.
    pub max_block_range: u64,
    /// Where a FRESH scan starts — used only when no cursor exists (a new
    /// database, or right after a re-index). When unset, the deployment block
    /// is detected by binary search over `eth_getCode` and cached.
    pub start_block: Option<u64>,
    /// How long readers may trust a report this loop makes. Set beside every
    /// target and renewed every poll interval and per committed chunk; a
    /// reader that finds it expired treats the index as stale. It has to
    /// cover a poll interval, a slow chunk and a missed cycle, which is why
    /// the CLI derives it from `poll_interval` when nothing sets it.
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

/// One chain's indexer: the store, the RPC endpoint and the knobs fused, so
/// the loop's operations take only what actually varies between calls.
pub struct Indexer {
    store: ChainStore,
    rpc: Url,
    config: IndexerConfig,
    filter: Filter,
}

impl Indexer {
    /// Build the indexer; the log filter is derived from the watched contracts
    /// once, here, instead of at every fetch.
    pub fn new(store: ChainStore, rpc: Url, config: IndexerConfig) -> Self {
        let watched: Vec<Address> = std::iter::once(config.contract)
            .chain(config.escrow)
            .collect();
        let filter = Filter::new().address(watched);
        Self {
            store,
            rpc,
            config,
            filter,
        }
    }

    /// Decode a log by the contract that emitted it: each contract's topics
    /// mean something only from that contract. The filter admits no other
    /// emitter, so one would be an RPC answering a different question.
    fn decode(
        &self,
        log: &Log,
    ) -> Result<Option<(NamesEvent, LogPosition)>, DecodeError> {
        let emitter = log.address();
        if emitter == self.config.contract {
            events::decode_registry(log)
        } else if Some(emitter) == self.config.escrow {
            events::decode_escrow(log)
        } else {
            tracing::warn!(
                %emitter,
                "a log from a contract the filter does not name; skipped"
            );
            Ok(None)
        }
    }

    /// Apply `logs` — every watched log past the cursor through block
    /// `through`, in chain order — and commit them with the cursor. An error
    /// leaves the cursor where it was. Returns how many events were NEW —
    /// replayed journal rows are skipped by the window and not counted, so
    /// the caller's log line stays honest.
    async fn commit(&self, logs: &[Log], through: u64) -> anyhow::Result<usize> {
        let mut window = self.store.begin_window().await?;
        let mut applied = 0usize;
        for log in logs {
            match self.decode(log) {
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

    /// Run until cancelled. Never returns early on an RPC or database error:
    /// those end the session, which logs, sleeps one interval, and opens
    /// another that resumes from the cursor.
    pub async fn run(self, cancel: CancellationToken) {
        tracing::info!(
            contract = %self.config.contract,
            escrow = ?self.config.escrow,
            chain_id = self.store.chain_id(),
            confirmations = self.config.confirmations,
            source = %self.config.source,
            "usernames indexer starting"
        );

        while !cancel.is_cancelled() {
            let session = async { Session::open(&self).await?.follow(&cancel).await };
            if let Err(e) = session.await {
                let error = format!("{e:#}");
                tracing::warn!(%error, "indexing stopped; resuming from the cursor");
                // Surfaced in /v1/status: a deterministic failure (an
                // undecodable log, say) stalls the cursor by design —
                // integrity over progress — and a stall must be visible
                // somewhere a monitor actually looks.
                self.store.set_window_error(Some(&error)).await;
                sleep_or_cancel(&cancel, self.config.poll_interval).await;
            }
        }
        tracing::info!("usernames indexer shutting down");
    }
}

/// One connection to the RPC, from the catch-up that opens it to the
/// cancellation or first error that ends it. Whatever a dropped socket or an
/// overflowing stream skipped, the next session's catch-up fetches.
struct Session<'a> {
    indexer: &'a Indexer,
    provider: RootProvider,
    feed: Feed,
    /// With a subscription, the head the last catch-up fetched through, until
    /// the next settle fetches past it.
    fetched_through: Option<u64>,
}

impl<'a> Session<'a> {
    /// Connect, and open what the configured source follows the chain with.
    async fn open(indexer: &'a Indexer) -> anyhow::Result<Self> {
        let source = indexer.config.source;
        let provider = source
            .connect(&indexer.rpc)
            .await
            .context("connecting to the RPC")?;
        let feed = Feed::open(source, &provider, &indexer.filter)
            .await
            .context("eth_subscribe")?;
        Ok(Self {
            indexer,
            provider,
            feed,
            fetched_through: None,
        })
    }

    /// Catch up, then wake every poll interval: a poll catches up again; a
    /// subscription reads the head when one is due or pushed logs wait for
    /// their confirmations, and otherwise only renews the report.
    async fn follow(mut self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let indexer = self.indexer;
        let config = &indexer.config;
        let mut vouched = self.catch_up(cancel).await?;
        let mut head_read = Instant::now();
        loop {
            if !self.feed.wait(config.poll_interval, cancel).await? {
                return Ok(());
            }
            let head_due = head_read.elapsed() >= config.head_interval;
            vouched = match &self.feed {
                Feed::Poll => self.catch_up(cancel).await?,
                Feed::Subscribe { unconfirmed, .. }
                    if head_due || !unconfirmed.is_empty() =>
                {
                    head_read = Instant::now();
                    self.settle().await?
                }
                // A renewal vouches only that the loop is alive and its stream
                // open, and only for a target that landed.
                Feed::Subscribe { .. } => {
                    if vouched {
                        indexer.store.touch_chain_target(config.stale_after).await;
                    }
                    vouched
                }
            };
        }
    }

    /// Read the head and commit every confirmed block past the cursor in
    /// `eth_getLogs` windows. A subscription fetches on up to the head and
    /// holds the unconfirmed tail, because its stream pushes only the blocks
    /// that arrive after it opened. Returns whether the target write landed,
    /// which is all a later renewal may vouch for.
    async fn catch_up(&mut self, cancel: &CancellationToken) -> anyhow::Result<bool> {
        let indexer = self.indexer;
        let config = &indexer.config;
        let (head, mut from) = tokio::try_join!(self.head(), self.next_block())?;
        let target = head.saturating_sub(config.confirmations);
        // Both, because they answer different questions: `head` is how far
        // the chain has grown, `target` is how far this loop intends to get.
        // A reader measuring staleness must use the second — the cursor is
        // never advanced past it.
        let ((), vouched) = tokio::join!(
            indexer.store.set_chain_head(head),
            indexer.store.set_chain_target(target, config.stale_after)
        );
        let through = match self.feed {
            Feed::Poll => target,
            Feed::Subscribe { .. } => {
                self.fetched_through = Some(head);
                head
            }
        };

        let mut total = 0usize;
        while from <= through && !cancel.is_cancelled() {
            let to = config.window_end(from, through);
            // Renewed before the fetch as well as after the commit: the fetch
            // is the slow part, and a report expiring during it would refuse
            // a loop that is busy catching up.
            if vouched {
                indexer.store.touch_chain_target(config.stale_after).await;
            }
            let logs = self.fetch(from, to).await?;
            let (confirmed, held): (Vec<Log>, Vec<Log>) = logs
                .into_iter()
                .partition(|log| log.block_number.is_some_and(|block| block <= target));
            if from <= target {
                let committed = to.min(target);
                total += indexer
                    .commit(&confirmed, committed)
                    .await
                    .with_context(|| format!("blocks {from}..{committed}"))?;
            }
            if let Some(unconfirmed) = self.feed.unconfirmed() {
                for log in held {
                    unconfirmed.push(log)?;
                }
            }
            // Each committed chunk is proof the loop is alive and the chain
            // reachable.
            if vouched {
                indexer.store.touch_chain_target(config.stale_after).await;
            }
            from = to + 1;
        }
        indexer.store.set_window_error(None).await;
        if total > 0 {
            tracing::info!(
                events = total,
                through_block = target.min(from.saturating_sub(1)),
                "indexed"
            );
        }
        Ok(vouched)
    }

    /// Read the head and commit through its confirmation depth from what the
    /// stream pushed. Each height holding pushed logs is checked against the
    /// canonical block there; heights without any cost nothing. Returns
    /// whether the target write landed.
    async fn settle(&mut self) -> anyhow::Result<bool> {
        let indexer = self.indexer;
        let (head, from) = tokio::try_join!(self.head(), self.next_block())?;
        let target = head.saturating_sub(indexer.config.confirmations);
        // Read after the head, so every log of a block below it is in hand,
        // however long the loop took to come round to the stream.
        self.feed.take_delivered()?;
        // On an endpoint that balances requests, the catch-up's head may have
        // come from a node behind the one streaming, and the blocks between
        // were neither fetched nor pushed.
        if let Some(fetched) = self.fetched_through.take() {
            self.hold(fetched + 1, head).await?;
        }
        let confirmed = self
            .feed
            .unconfirmed()
            .map(|unconfirmed| unconfirmed.confirmed(target))
            .unwrap_or_default();

        let blocks = confirmed
            .into_iter()
            .filter(|(height, _)| *height >= from)
            .map(|(height, forks)| self.canonical(height, forks));
        let ((), logs) =
            tokio::join!(indexer.store.set_chain_head(head), try_join_all(blocks));
        let logs: Vec<Log> = logs?.into_iter().flatten().collect();
        if from <= target {
            let applied = indexer
                .commit(&logs, target)
                .await
                .with_context(|| format!("blocks {from}..{target}"))?;
            if applied > 0 {
                tracing::info!(events = applied, through_block = target, "indexed");
            }
        }
        // Written after the commit, unlike a catch-up's target: nothing is
        // left to catch up, so no reader sees the target ahead of the cursor.
        let (vouched, ()) = tokio::join!(
            indexer
                .store
                .set_chain_target(target, indexer.config.stale_after),
            indexer.store.set_window_error(None)
        );
        Ok(vouched)
    }

    /// Fetch blocks `from..=to` in `eth_getLogs` windows and hold their logs
    /// beside the pushed ones, for the canonical check to settle alike.
    async fn hold(&mut self, from: u64, to: u64) -> anyhow::Result<()> {
        let mut start = from;
        while start <= to {
            let end = self.indexer.config.window_end(start, to);
            let logs = self.fetch(start, end).await?;
            if let Some(unconfirmed) = self.feed.unconfirmed() {
                for log in logs {
                    unconfirmed.push(log)?;
                }
            }
            start = end + 1;
        }
        Ok(())
    }

    /// The logs of the canonical block at `height`: the pushed ones when the
    /// stream saw that block, or fetched by its hash when a reorg replaced
    /// the one it saw.
    async fn canonical(&self, height: u64, forks: Forks) -> anyhow::Result<Vec<Log>> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(height))
            .await
            .with_context(|| format!("eth_getBlockByNumber {height}"))?
            .with_context(|| format!("block {height} is confirmed but not served"))?;
        let hash = block.header.hash;
        if let Some(logs) = forks.into_canonical(hash) {
            return Ok(logs);
        }
        tracing::info!(
            height,
            %hash,
            "a reorg replaced the block the stream pushed logs from; refetching"
        );
        let mut logs = self
            .provider
            .get_logs(&self.indexer.filter.clone().at_block_hash(hash))
            .await
            .with_context(|| format!("eth_getLogs at block {hash}"))?;
        anyhow::ensure!(
            logs.iter().all(|log| log.block_number == Some(height)),
            "eth_getLogs at block {hash} answered a log from another height"
        );
        logs.sort_by_key(|log| log.log_index);
        Ok(logs)
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
        let mut logs = chain::fetch_logs(&self.provider, &self.indexer.filter, from, to)
            .await
            .with_context(|| format!("eth_getLogs {from}..{to}"))?;
        // Every log has to sit in the window: where it sits decides whether
        // it commits now or waits for its confirmations.
        anyhow::ensure!(
            logs.iter().all(|log| log
                .block_number
                .is_some_and(|block| (from..=to).contains(&block))),
            "eth_getLogs {from}..{to} answered a log outside the window"
        );
        logs.sort_by_key(|log| (log.block_number, log.log_index));

        // The logs and the head estimate may have come from different
        // backends behind one RPC URL. Before treating this window as final,
        // require the node answering NOW to have reached its end; a lagging
        // replica that returned empty logs for blocks it never saw must fail
        // the window, not advance the cursor past events. (A single
        // well-behaved endpoint makes this check free; a load balancer makes
        // it necessary but not sufficient — see the README.)
        let seen = self
            .provider
            .get_block_number()
            .await
            .context("eth_blockNumber recheck")?;
        if seen < to {
            anyhow::bail!("RPC backend reports block {seen}, behind window end {to}");
        }
        Ok(logs)
    }

    /// The first block past the cursor, or where a fresh scan starts.
    async fn next_block(&self) -> anyhow::Result<u64> {
        match self
            .indexer
            .store
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
        let contract = indexer.config.contract;
        if let Some(block) = indexer.config.start_block {
            return Some(block);
        }
        match indexer.store.deploy_block(contract).await {
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
        match chain::find_deployment_block(&self.provider, contract, latest).await {
            Ok(Some(block)) => {
                tracing::info!(block, "detected contract deployment block");
                if let Err(e) = indexer.store.set_deploy_block(contract, block).await {
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

async fn sleep_or_cancel(cancel: &CancellationToken, interval: Duration) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(interval) => {}
    }
}
