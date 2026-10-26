//! The subscription source against a real chain. anvil streams the libid
//! contracts' logs over a WebSocket, through a relay that can drop the
//! socket, lag a head read and records every call the indexer makes, so each
//! test checks what the indexer committed and what the RPC was asked for it.
//!
//! Skips without `DATABASE_URL` or `anvil`, unless `CI` is set: there a
//! missing one fails. Everything past those checks panics on failure.

use std::time::Duration;

use alloy::{
    primitives::{
        Address,
        U256,
    },
    providers::{
        ext::AnvilApi,
        Provider,
    },
    rpc::types::anvil::{
        ReorgOptions,
        TransactionData,
    },
};
use axum::http::StatusCode;
use chrono::TimeDelta;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        HandleHistory,
        HandleResolution,
        HistoryEvent,
        Unclaimed,
    },
    db::{
        self,
        ChainStore,
        IndexPosition,
        WriterLease,
    },
    indexer::{
        Indexer,
        IndexerConfig,
        LogSource,
    },
    nodes::KnownPlatform,
};

mod common;
mod relay;
use common::libid::{
    Disclose,
    Handle,
    Libid,
    Node,
    Proof,
};
use relay::Relay;

/// How long any wait on the indexer may take before the test fails.
const PATIENCE: Duration = Duration::from_secs(30);

/// One test's chain: anvil with the libid contracts deployed, the relay the
/// indexer dials, and the store it writes under the chain's writer lease.
/// Reads without a chain span every suite's rows, so the handles and
/// addresses this suite binds are its own.
struct Chain {
    node: Node,
    libid: Libid,
    relay: Relay,
    pool: PgPool,
    store: ChainStore,
    writer: WriterLease,
}

/// The indexer task a test runs, and the token that stops it.
struct Running {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// What a proof that `handle` on X is held reports: the handle doubles as
/// the account id.
fn x_proof(handle: &'static str) -> Proof {
    Proof {
        handle: Handle(KnownPlatform::X, handle),
        id: handle,
        observed_at: 1000,
    }
}

impl Chain {
    /// A fresh chain with id `chain_id` and its rows cleared, or `None` where
    /// the suite skips.
    async fn start(chain_id: u64) -> Option<Self> {
        let Some(url) = common::database_url() else {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        };
        if !common::anvil_available() {
            eprintln!("skipping: anvil not on PATH");
            return None;
        }

        let pool = db::connect_and_migrate(&url).await.expect("database");
        common::clear_chain(&pool, chain_id as i64).await;
        let store = ChainStore::new(pool.clone(), chain_id as i64);

        let node = Node::spawn(chain_id);
        let libid = Libid::deploy(&node.provider, node.accounts()[0]).await;
        // A few blocks of history, so the catch-up spans more than a window.
        node.provider.anvil_mine(Some(8), None).await.expect("mine");
        let relay = Relay::start(node.ws()).await;

        let writer = store.acquire_writer().await.expect("writer lease");
        store
            .prepare(
                &writer,
                *libid.registry.address(),
                Some(*libid.escrow.address()),
            )
            .await
            .expect("prepare");
        Some(Self {
            node,
            libid,
            relay,
            pool,
            store,
            writer,
        })
    }

    /// The suite's indexer settings: a log subscription over the relay,
    /// `confirmations` behind the head, on short intervals.
    fn config(&self, confirmations: u64) -> IndexerConfig {
        IndexerConfig {
            contract: *self.libid.registry.address(),
            escrow: Some(*self.libid.escrow.address()),
            confirmations,
            source: LogSource::Subscribe,
            poll_interval: Duration::from_millis(100),
            head_interval: Duration::from_millis(300),
            max_block_range: 5,
            start_block: Some(0),
            stale_after: Duration::from_secs(120),
        }
    }

    /// Run the indexer over the relay.
    fn run(&self, config: IndexerConfig) -> Running {
        let cancel = CancellationToken::new();
        let indexer = Indexer::new(self.store.clone(), self.relay.url(), config);
        let task = tokio::spawn(indexer.run(cancel.clone()));
        Running { cancel, task }
    }

    /// The dev accounts that act in a test, after the deployer.
    fn accounts(&self) -> &[Address] {
        &self.node.accounts()[1..]
    }

    /// The chain head.
    async fn head(&self) -> u64 {
        self.node.provider.get_block_number().await.expect("head")
    }

    /// Mine `blocks` empty blocks.
    async fn mine(&self, blocks: u64) {
        self.node
            .provider
            .anvil_mine(Some(blocks), None)
            .await
            .expect("mine");
    }

    /// `holder` binds `handle` on X privately; the block the bind landed in.
    async fn bind(&mut self, handle: &'static str, holder: Address) -> u64 {
        self.libid
            .bind(holder, x_proof(handle), Disclose::Nothing, U256::ZERO)
            .await
    }

    /// `payer` escrows a native deposit for `handle` on X, which nobody
    /// holds yet; the block it landed in.
    async fn deposit(&self, handle: &'static str, payer: Address) -> u64 {
        self.libid
            .deposit(payer, Handle(KnownPlatform::X, handle), 100)
            .await
    }

    /// Where the index stands.
    async fn position(&self) -> IndexPosition {
        self.store.index_position().await.expect("position")
    }

    /// Wait until the index stands at `block` or past it: the cursor
    /// committed through it, and the target reported beside it.
    async fn wait_for_index(&self, block: u64) {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let position = self.position().await;
            let reached = |at: Option<u64>| at.is_some_and(|at| at >= block);
            if reached(position.cursor) && reached(position.target) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the index did not reach block {block}: {position:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for the indexer's next head read. After a catch-up that is the
    /// session's first settle, which fetches whatever the catch-up's head
    /// may have missed: nothing, when no block arrived in between.
    async fn wait_for_head_read(&self) {
        let reads = self.called("eth_blockNumber");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while self.called("eth_blockNumber") == reads {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the indexer read no head"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Who `handle` on X resolves to, or the refusal's status.
    async fn resolve(&self, handle: &str) -> Result<Address, StatusCode> {
        let reply = common::get_on::<HandleResolution>(
            &self.pool,
            self.store.chain_id(),
            &format!("/v1/resolve/handle/x/{handle}"),
        )
        .await;
        if reply.status.is_success() {
            Ok(reply.answer().bindings[0].owner)
        } else {
            Err(reply.refusal().0)
        }
    }

    /// The events of `handle` on X, oldest first, each with its block.
    async fn history(&self, handle: &str) -> Vec<(u64, HistoryEvent)> {
        let history: HandleHistory = common::get_on(
            &self.pool,
            self.store.chain_id(),
            &format!("/v1/history/handle/x/{handle}"),
        )
        .await
        .answer();
        history
            .entries
            .into_iter()
            .rev()
            .map(|entry| (entry.block_number as u64, entry.event))
            .collect()
    }

    /// Every escrow slot still holding something on this chain.
    async fn unclaimed(&self) -> Unclaimed {
        common::get_on(&self.pool, self.store.chain_id(), "/v1/escrow/unclaimed")
            .await
            .answer()
    }

    /// How many times the indexer called `method` through the relay.
    fn called(&self, method: &str) -> usize {
        self.relay
            .calls()
            .iter()
            .filter(|called| called.as_str() == method)
            .count()
    }

    /// Stop the indexer and give the writer lease back.
    async fn stop(self, running: Running) {
        running.cancel.cancel();
        running.task.await.expect("the indexer stops cleanly");
        self.writer.release().await.expect("the lease releases");
    }
}

/// With no events, the cursor still follows the head, and following it
/// costs nothing but head reads: no window is fetched and no block read.
#[tokio::test]
async fn with_no_events_the_cursor_follows_the_head_on_head_reads_alone() {
    let Some(chain) = Chain::start(43120).await else {
        return;
    };
    let running = chain.run(chain.config(2));
    chain.wait_for_index(chain.head().await - 2).await;
    chain.wait_for_head_read().await;
    let windows = chain.called("eth_getLogs");

    chain.mine(10).await;
    let target = chain.head().await - 2;
    chain.wait_for_index(target).await;

    assert_eq!(
        chain.called("eth_getLogs"),
        windows,
        "{:?}",
        chain.relay.calls()
    );
    assert_eq!(chain.called("eth_getBlockByNumber"), 0);
    let position = chain.position().await;
    assert_eq!(position.target, Some(target));
    assert_eq!(position.cursor, Some(target));
    assert!(
        position
            .valid_for
            .is_some_and(|valid_for| valid_for > TimeDelta::zero()),
        "{position:?}"
    );
    chain.stop(running).await;
}

/// Between head reads the report is renewed from the store alone, so an idle
/// indexer stays trusted for longer than one report lasts without asking the
/// RPC anything.
#[tokio::test]
async fn between_head_reads_the_report_is_renewed_without_the_rpc() {
    let Some(chain) = Chain::start(43125).await else {
        return;
    };
    let running = chain.run(IndexerConfig {
        head_interval: Duration::from_secs(3600),
        stale_after: Duration::from_secs(2),
        ..chain.config(2)
    });
    chain.wait_for_index(chain.head().await - 2).await;
    let reported = chain.position().await.reported_at;
    let calls = chain.relay.calls().len();

    tokio::time::sleep(Duration::from_secs(3)).await;

    let position = chain.position().await;
    assert!(
        position
            .valid_for
            .is_some_and(|valid_for| valid_for > TimeDelta::zero()),
        "{position:?}"
    );
    assert!(position.reported_at > reported, "{position:?}");
    assert_eq!(
        chain.relay.calls().len(),
        calls,
        "{:?}",
        chain.relay.calls()
    );
    chain.stop(running).await;
}

/// A pushed event commits once its block is confirmed, both watched
/// contracts' alike, and costs one canonical block read per block holding
/// events rather than any window.
#[tokio::test]
async fn pushed_events_commit_once_confirmed_at_one_block_read_each() {
    let Some(mut chain) = Chain::start(43121).await else {
        return;
    };
    let running = chain.run(chain.config(2));
    chain.wait_for_index(chain.head().await - 2).await;
    chain.wait_for_head_read().await;
    let windows = chain.called("eth_getLogs");

    let (holder, payer) = (chain.accounts()[0], chain.accounts()[1]);
    let bound = chain.bind("pushed_1", holder).await;
    let deposited = chain.deposit("deposited_1", payer).await;
    chain.mine(2).await;
    chain.wait_for_index(deposited).await;

    assert_eq!(chain.resolve("pushed_1").await, Ok(holder));
    let history = chain.history("pushed_1").await;
    assert!(
        matches!(
            history[..],
            [
                (bound_at, HistoryEvent::IdentityBound { .. }),
                (ceremony_at, HistoryEvent::CeremonyBound { .. }),
            ] if bound_at == bound && ceremony_at == bound
        ),
        "{history:?}"
    );
    let history = chain.history("deposited_1").await;
    assert!(
        matches!(
            history[..],
            [(block, HistoryEvent::Deposited { .. })] if block == deposited
        ),
        "{history:?}"
    );
    assert_eq!(
        chain.called("eth_getLogs"),
        windows,
        "{:?}",
        chain.relay.calls()
    );
    assert_eq!(chain.called("eth_getBlockByNumber"), 2);
    chain.stop(running).await;
}

/// Logs emitted while the socket is down are never pushed; the next session
/// fetches them from the cursor, once. The relay holds the reconnect until
/// it reopens, so a client that reconnected and re-subscribed on its own,
/// skipping the gap, would fail here.
#[tokio::test]
async fn a_dropped_socket_is_backfilled_from_the_cursor_on_reconnect() {
    let Some(mut chain) = Chain::start(43122).await else {
        return;
    };
    let running = chain.run(chain.config(2));
    chain.wait_for_index(chain.head().await - 2).await;
    // Past the first settle, whose fetch past the catch-up's head would
    // find the gap too.
    chain.wait_for_head_read().await;

    // The stream is gone before the event is emitted, so only a backfill
    // can find it.
    chain.relay.cut().await;
    let holder = chain.accounts()[0];
    let bound = chain.bind("backfilled_1", holder).await;
    chain.mine(3).await;
    chain.relay.reopen();
    chain.wait_for_index(bound).await;

    assert_eq!(chain.resolve("backfilled_1").await, Ok(holder));
    let history = chain.history("backfilled_1").await;
    assert!(
        matches!(
            history[..],
            [
                (bound_at, HistoryEvent::IdentityBound { .. }),
                (ceremony_at, HistoryEvent::CeremonyBound { .. }),
            ] if bound_at == bound && ceremony_at == bound
        ),
        "{history:?}"
    );
    assert_eq!(
        chain.called("eth_subscribe"),
        2,
        "{:?}",
        chain.relay.calls()
    );
    chain.stop(running).await;
}

/// A head read answered from behind the node that streams leaves a block
/// neither fetched nor pushed; the session's first settle fetches past the
/// catch-up's head and finds it.
#[tokio::test]
async fn a_block_behind_a_lagging_head_read_is_fetched_at_the_first_settle() {
    let Some(mut chain) = Chain::start(43124).await else {
        return;
    };
    let holder = chain.accounts()[0];
    let missed = chain.bind("lagged_1", holder).await;
    chain.relay.lag_next_head(2);

    let running = chain.run(chain.config(1));
    // The catch-up's head was two short: it stopped before the binding,
    // which was mined before the stream opened.
    chain.wait_for_index(missed - 3).await;
    chain.mine(1).await;
    chain.wait_for_index(missed).await;

    assert_eq!(chain.resolve("lagged_1").await, Ok(holder));
    chain.stop(running).await;
}

/// A reorg that replaces the blocks above the confirmation depth drops the
/// deposit a replaced block held, and keeps the bind its replacement holds:
/// the canonical block at each height decides, read once that height is
/// confirmed.
#[tokio::test]
async fn a_reorg_at_the_confirmation_depth_drops_the_event_it_orphaned() {
    let Some(mut chain) = Chain::start(43123).await else {
        return;
    };
    let running = chain.run(chain.config(3));
    chain.wait_for_index(chain.head().await - 3).await;
    chain.wait_for_head_read().await;
    let windows = chain.called("eth_getLogs");

    let (payer, heir) = (chain.accounts()[0], chain.accounts()[1]);
    // The heir's proof is set up below the blocks the reorg replaces, so the
    // block replacing them holds the whole bind.
    let heirs_proof = x_proof("replacing_1");
    let payload = chain.libid.prove(heir, heirs_proof).await;
    let orphaned = chain.deposit("orphaned_1", payer).await;
    chain.mine(2).await;
    // Pushed and held, but two blocks deep: not yet confirmed.
    let cursor = chain.store.cursor().await.expect("cursor");
    assert!(cursor.is_some_and(|cursor| cursor < orphaned), "{cursor:?}");
    let binding = chain.libid.binding(
        heir,
        heirs_proof.handle,
        payload,
        Disclose::Nothing,
        U256::ZERO,
    );
    chain
        .node
        .provider
        .anvil_reorg(ReorgOptions {
            depth: 3,
            tx_block_pairs: vec![(TransactionData::JSON(binding), 1)],
        })
        .await
        .expect("reorg");
    chain.mine(2).await;
    chain.wait_for_index(orphaned + 1).await;

    assert_eq!(chain.resolve("replacing_1").await, Ok(heir));
    assert!(chain.history("orphaned_1").await.is_empty());
    let unclaimed = chain.unclaimed().await;
    assert!(unclaimed.amounts.is_empty(), "{unclaimed:?}");
    let history = chain.history("replacing_1").await;
    assert!(
        matches!(
            history[..],
            [
                (bound_at, HistoryEvent::IdentityBound { .. }),
                (ceremony_at, HistoryEvent::CeremonyBound { .. }),
            ] if bound_at == orphaned + 1 && ceremony_at == orphaned + 1
        ),
        "{history:?}"
    );
    // One refetch: the height whose pushed block the reorg replaced.
    assert_eq!(
        chain.called("eth_getLogs"),
        windows + 1,
        "{:?}",
        chain.relay.calls()
    );
    chain.stop(running).await;
}
