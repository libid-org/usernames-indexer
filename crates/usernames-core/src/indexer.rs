//! The indexing loop: catch up with `eth_getLogs` windows, then follow the
//! chain, by polling or from pushed heads and logs.
//!
//! Each window commits through [`crate::db::Window`] — journal rows, projection
//! writes and the cursor together — so a crash replays a whole window instead
//! of resuming inside one, and the journal's conflict gates every write, so
//! the replay converges.

mod source;

use std::time::Duration;

use alloy::{
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
use tokio_util::sync::CancellationToken;
use url::Url;

use self::source::{
    Feed,
    Wake,
};
pub use self::source::{
    LogSource,
    UnknownLogSource,
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
    /// How the loop learns of new blocks and logs.
    pub source: LogSource,
    /// The loop's cadence: a poll cycle, or with a subscription the renewal
    /// of the report while no head arrives. Also the retry delay after a
    /// failure.
    pub poll_interval: Duration,
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

    /// `error` with the RPC endpoint named by its setting. An HTTP client's
    /// error carries the URL it called, an endpoint's URL often holds its
    /// API key, and `/v1/status` shows the stored error to anyone. Matched
    /// past the scheme, so the WebSocket twin is caught too.
    fn redacted(&self, error: String) -> String {
        let endpoint = &self.rpc.as_str()[self.rpc.scheme().len()..];
        error.replace(endpoint, "://<RPC_URL>")
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
                let error = self.redacted(format!("{e:#}"));
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
        })
    }

    /// Catch up, then follow: a poll catches up again every interval; a
    /// subscription commits through each pushed head's confirmation depth,
    /// fetching only the blocks a pushed log or the catch-up left unread.
    async fn follow(mut self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let config = &self.indexer.config;
        let (mut vouched, caught_up_to) = self.catch_up(cancel).await?;
        // Blocks up to here may hold logs nobody fetched: the catch-up stopped
        // at its confirmation depth, and a block mined before the streams
        // opened was never pushed. The first pushed head extends it, since an
        // endpoint behind a load balancer may have read the catch-up's head
        // from a lagging node.
        let mut unread = Some(caught_up_to);
        let mut first_head = true;
        loop {
            match self.feed.wait(config.poll_interval, cancel).await? {
                Wake::Cancelled => return Ok(()),
                Wake::Tick => match self.feed {
                    Feed::Poll => (vouched, _) = self.catch_up(cancel).await?,
                    // No head in a whole interval: the stream is open, so the
                    // loop vouches it is alive, for a target that landed.
                    Feed::Subscribe { .. } => {
                        if vouched {
                            self.indexer
                                .store
                                .touch_chain_target(config.stale_after)
                                .await;
                        }
                    }
                },
                Wake::Log(block) => unread = unread.max(Some(block)),
                Wake::Head(head) => {
                    if first_head {
                        unread = unread.max(Some(head));
                        first_head = false;
                    }
                    vouched = self.advance(head, &mut unread).await?;
                }
            }
        }
    }

    /// Read the head and commit every confirmed block past the cursor in
    /// `eth_getLogs` windows. Returns whether the target write landed, which
    /// is all a later renewal may vouch for, and the head it read.
    async fn catch_up(
        &mut self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(bool, u64)> {
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

        let mut total = 0usize;
        while from <= target && !cancel.is_cancelled() {
            let to = config.window_end(from, target);
            // Renewed before the fetch as well as after the commit: the fetch
            // is the slow part, and a report expiring during it would refuse
            // a loop that is busy catching up.
            if vouched {
                indexer.store.touch_chain_target(config.stale_after).await;
            }
            total += self.commit_fetched(from, to).await?;
            from = to + 1;
        }
        indexer.store.set_window_error(None).await;
        if total > 0 {
            tracing::info!(events = total, through_block = target, "indexed");
        }
        Ok((vouched, head))
    }

    /// Commit through `head`'s confirmation depth. Blocks up to `unread` may
    /// hold logs not yet fetched, so they are; the cursor passes any other
    /// block without a call. Returns whether the target write landed.
    async fn advance(
        &mut self,
        head: u64,
        unread: &mut Option<u64>,
    ) -> anyhow::Result<bool> {
        let indexer = self.indexer;
        let config = &indexer.config;
        let target = head.saturating_sub(config.confirmations);
        let from = self.next_block().await?;
        if from <= target {
            if unread.is_some_and(|unread| unread >= from) {
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
            if unread.is_some_and(|unread| unread <= target) {
                *unread = None;
            }
        }
        let ((), vouched, ()) = tokio::join!(
            indexer.store.set_chain_head(head),
            indexer.store.set_chain_target(target, config.stale_after),
            indexer.store.set_window_error(None),
        );
        Ok(vouched)
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
        let mut logs = chain::fetch_logs(&self.provider, &self.indexer.filter, from, to)
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
                if let Err(e) = indexer.store.set_deploy_block(block).await {
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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    #[tokio::test]
    async fn a_stored_error_names_the_rpc_url_by_its_setting() {
        let pool = PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let indexer = Indexer::new(
            ChainStore::new(pool, 1),
            "https://eth-mainnet.g.alchemy.com/v2/secret"
                .parse()
                .unwrap(),
            IndexerConfig {
                contract: Address::ZERO,
                escrow: None,
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
