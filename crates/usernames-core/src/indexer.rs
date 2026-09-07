//! The polling loop: window the chain, decode, apply, advance.
//!
//! Each window commits through [`db::Window`] — journal rows, projection
//! writes and the cursor together — so a crash replays a whole window instead
//! of resuming inside one, and every write in the window is an idempotent
//! upsert so the replay converges.

use alloy::{
    primitives::Address,
    providers::Provider,
    rpc::types::Filter,
};
use tokio_util::sync::CancellationToken;
use tracing::{
    info,
    warn,
};

use crate::{
    chain,
    db::ChainStore,
    events,
};

/// The loop's knobs, resolved by the CLI. The chain itself is carried by the
/// [`ChainStore`] the [`Indexer`] is built over.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// The IdentityNames ERC1967 proxy — the address to watch. The
    /// implementation behind it changes on upgrade; the proxy is the one that
    /// emits.
    pub contract: Address,
    /// Blocks behind the head this indexer stays. The loop never scans past
    /// `latest - confirmations`, so a reorg shallower than this cannot leave
    /// the read model holding events the canonical chain never emitted.
    pub confirmations: u64,
    /// Seconds between poll cycles; also the retry delay after a failure.
    pub poll_interval_secs: u64,
    /// Largest `eth_getLogs` window, sized to the RPC provider's limits.
    pub max_block_range: u64,
    /// Where a FRESH scan starts — used only when no cursor exists (a new
    /// database, or right after a re-index). When unset, the deployment block
    /// is detected by binary search over `eth_getCode` and cached.
    pub start_block: Option<u64>,
    /// How long readers may trust a report this loop makes, in seconds. Set
    /// beside every target and renewed per committed chunk; a reader that
    /// finds it expired treats the mirror as stale. It has to cover a poll
    /// interval, a slow chunk and a missed cycle, which is why the CLI derives
    /// it from `poll_interval_secs` when nothing sets it.
    pub stale_after_secs: u64,
}

/// One chain's indexer: the store, the RPC provider and the knobs fused, so
/// the loop's operations take only what actually varies between calls.
pub struct Indexer<P> {
    store: ChainStore,
    provider: P,
    config: IndexerConfig,
    filter: Filter,
}

impl<P: Provider> Indexer<P> {
    /// Build the indexer; the log filter is derived from the watched contract
    /// once, here, instead of at every fetch.
    pub fn new(store: ChainStore, provider: P, config: IndexerConfig) -> Self {
        let filter = Filter::new().address(config.contract);
        Self {
            store,
            provider,
            config,
            filter,
        }
    }

    /// Where a fresh scan starts: the override, the cached detection, or a
    /// new binary search. `None` means the RPC failed mid-detection — the
    /// caller retries next cycle instead of committing to a guess. Only a
    /// genuine detection is cached: a cached fallback would outlive the
    /// failure that produced it and pin this deployment to genesis scans
    /// forever.
    async fn resolve_start_block(&self) -> Option<u64> {
        if let Some(block) = self.config.start_block {
            return Some(block);
        }
        match self.store.deploy_block(self.config.contract).await {
            Ok(Some(block)) => return Some(block),
            Ok(None) => {}
            Err(e) => warn!(%e, "failed to read the cached deployment block"),
        }
        let latest = match self.provider.get_block_number().await {
            Ok(n) => n,
            Err(e) => {
                warn!(%e, "no latest block for deployment detection; retrying next cycle");
                return None;
            }
        };
        match chain::find_deployment_block(&self.provider, self.config.contract, latest)
            .await
        {
            Ok(Some(block)) => {
                info!(block, "detected contract deployment block");
                if let Err(e) = self
                    .store
                    .set_deploy_block(self.config.contract, block)
                    .await
                {
                    warn!(%e, "failed to cache the deployment block");
                }
                Some(block)
            }
            Ok(None) => {
                // Absence is an answer, but not one worth caching: the
                // contract may simply not be deployed yet. Scanning from
                // genesis is slow but never wrong, and the next fresh cycle
                // re-checks.
                warn!(
                    "the contract has no code at the chain head; scanning from genesis"
                );
                Some(0)
            }
            Err(e) => {
                warn!(%e, "deployment detection failed on RPC; retrying next cycle");
                None
            }
        }
    }

    /// One chunk: fetch, decode, apply, commit — cursor included. An error
    /// leaves the cursor where it was, so the same blocks retry next cycle.
    /// Returns how many events were NEW — replayed journal rows are skipped
    /// by the window and not counted, so the caller's log line stays honest.
    async fn process_chunk(&self, from: u64, to: u64) -> anyhow::Result<usize> {
        let mut logs = chain::fetch_logs(&self.provider, &self.filter, from, to)
            .await
            .map_err(|e| anyhow::anyhow!("eth_getLogs {from}..{to}: {e}"))?;
        logs.sort_by_key(|l| (l.block_number.unwrap_or(0), l.log_index.unwrap_or(0)));

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
            .map_err(|e| anyhow::anyhow!("eth_blockNumber recheck: {e}"))?;
        if seen < to {
            anyhow::bail!("RPC backend reports block {seen}, behind window end {to}");
        }

        let mut window = self.store.begin_window().await?;
        let mut applied = 0usize;
        for log in &logs {
            match events::decode(log) {
                Ok(Some((event, position))) => {
                    if window.apply(&event, &position).await? {
                        applied += 1;
                    }
                }
                Ok(None) => {
                    // A topic this build does not know. The contract is
                    // upgradeable, so this is survivable — but it is not
                    // silent, because a new event type usually means the read
                    // model is due for a version bump.
                    warn!(
                        topic0 = ?log.topic0(),
                        block = log.block_number,
                        "unknown event topic from the contract; skipped"
                    );
                }
                Err(e) => {
                    anyhow::bail!("undecodable log at block {:?}: {e}", log.block_number)
                }
            }
        }
        window.commit(to).await?;
        Ok(applied)
    }

    /// Run until cancelled. Never returns early on an RPC or database error:
    /// those log, sleep one interval, and retry the same window.
    pub async fn run(self, cancel: CancellationToken) {
        info!(
            contract = %self.config.contract,
            chain_id = self.store.chain_id(),
            confirmations = self.config.confirmations,
            "usernames indexer starting"
        );

        loop {
            if cancel.is_cancelled() {
                info!("usernames indexer shutting down");
                return;
            }

            let from_block = match self.store.cursor().await {
                Ok(Some(cursor)) => cursor + 1,
                Ok(None) => match self.resolve_start_block().await {
                    Some(block) => block,
                    None => {
                        sleep_or_cancel(&cancel, self.config.poll_interval_secs).await;
                        continue;
                    }
                },
                Err(e) => {
                    warn!(%e, "failed to read the cursor");
                    sleep_or_cancel(&cancel, self.config.poll_interval_secs).await;
                    continue;
                }
            };

            let latest = match self.provider.get_block_number().await {
                Ok(n) => n,
                Err(e) => {
                    warn!(%e, "failed to get the latest block");
                    sleep_or_cancel(&cancel, self.config.poll_interval_secs).await;
                    continue;
                }
            };
            let head = latest.saturating_sub(self.config.confirmations);
            self.store.set_chain_head(latest).await;
            // Both, because they answer different questions: `head` is how far
            // the chain has grown, `target` is how far this loop intends to
            // get. A reader measuring staleness must use the second — the
            // cursor is never advanced past it.
            self.store
                .set_chain_target(head, self.config.stale_after_secs)
                .await;

            if from_block > head {
                sleep_or_cancel(&cancel, self.config.poll_interval_secs).await;
                continue;
            }

            let mut chunk_from = from_block;
            let mut total = 0usize;
            let mut failed = false;
            while chunk_from <= head && !cancel.is_cancelled() {
                let chunk_to = chunk_from
                    .saturating_add(self.config.max_block_range.max(1) - 1)
                    .min(head);
                match self.process_chunk(chunk_from, chunk_to).await {
                    Ok(applied) => {
                        total += applied;
                        chunk_from = chunk_to + 1;
                        // Each committed chunk is proof the loop is alive and
                        // the chain reachable. Without this a long catch-up
                        // would expire under the API's gate.
                        self.store
                            .touch_chain_target(self.config.stale_after_secs)
                            .await;
                    }
                    Err(e) => {
                        warn!(%e, chunk_from, chunk_to, "window failed; will retry");
                        // Surfaced in /v1/status: a deterministic failure here
                        // (an undecodable log, say) stalls the cursor by
                        // design — integrity over progress — and a stall must
                        // be visible somewhere a monitor actually looks.
                        self.store
                            .set_window_error(Some(&format!(
                                "blocks {chunk_from}..{chunk_to}: {e}"
                            )))
                            .await;
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                self.store.set_window_error(None).await;
            }
            if total > 0 {
                info!(
                    events = total,
                    through_block = head.min(chunk_from.saturating_sub(1)),
                    "indexed"
                );
            }

            sleep_or_cancel(&cancel, self.config.poll_interval_secs).await;
        }
    }
}

async fn sleep_or_cancel(cancel: &CancellationToken, secs: u64) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
    }
}
