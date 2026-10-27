//! The subscription source against a real chain. anvil pushes heads over a
//! WebSocket, through a relay that can drop the socket or silence it and
//! records every call the indexer makes, so each test checks what the
//! indexer committed and what the RPC was asked for it.
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
};
use axum::http::StatusCode;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        HandleHistory,
        HandleResolution,
        HistoryEvent,
    },
    db::{
        self,
        ChainStore,
        ChainWriter,
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
    writer: ChainWriter,
    lease: WriterLease,
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

        let lease = store.acquire_writer().await.expect("writer lease");
        let writer = store
            .prepare(
                &lease,
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
            writer,
            lease,
        })
    }

    /// The suite's indexer settings: a head subscription over the relay,
    /// `confirmations` behind the head, on short intervals.
    fn config(&self, confirmations: u64) -> IndexerConfig {
        IndexerConfig {
            contract: *self.libid.registry.address(),
            escrow: Some(*self.libid.escrow.address()),
            confirmations,
            source: LogSource::Subscribe,
            poll_interval: Duration::from_millis(100),
            max_block_range: 5,
            start_block: Some(0),
            stale_after: Duration::from_secs(120),
        }
    }

    /// Run the indexer over the relay.
    fn run(&self, config: IndexerConfig) -> Running {
        let cancel = CancellationToken::new();
        let indexer = Indexer::new(self.writer.clone(), self.relay.url(), config);
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

    /// The last window failure the indexer recorded, while it stands.
    async fn window_error(&self) -> Option<String> {
        self.writer
            .store()
            .state()
            .await
            .expect("the chain's row")
            .window_error
    }

    /// Where the index stands.
    async fn position(&self) -> IndexPosition {
        self.writer
            .store()
            .index_position()
            .await
            .expect("position")
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

    /// Who `handle` on X resolves to, or the refusal's status.
    async fn resolve(&self, handle: &str) -> Result<Address, StatusCode> {
        let reply = common::get_on::<HandleResolution>(
            &self.pool,
            self.writer.store().chain_id(),
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
            self.writer.store().chain_id(),
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
        self.lease.release().await.expect("the lease releases");
    }
}

/// A pushed event commits once its block is confirmed, both watched
/// contracts' alike; blocks without events move the cursor on their pushed
/// heads alone, with nothing fetched.
#[tokio::test]
async fn pushed_events_commit_once_confirmed_and_empty_blocks_cost_no_fetch() {
    let Some(mut chain) = Chain::start(43121).await else {
        return;
    };
    let running = chain.run(chain.config(2));
    chain.wait_for_index(chain.head().await - 2).await;

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

    let windows = chain.called("eth_getLogs");
    chain.mine(5).await;
    chain.wait_for_index(chain.head().await - 2).await;
    assert_eq!(
        chain.called("eth_getLogs"),
        windows,
        "{:?}",
        chain.relay.calls()
    );
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
    // Past the first pushed head, whose fetch from the cursor would find the
    // gap too.
    chain.mine(1).await;
    chain.wait_for_index(chain.head().await - 2).await;

    // The stream is gone before the event is emitted, so only the next
    // session's catch-up can find it.
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
    // Heads, once per session.
    assert_eq!(
        chain.called("eth_subscribe"),
        2,
        "{:?}",
        chain.relay.calls()
    );
    chain.stop(running).await;
}

/// A socket that stays open but pushes nothing would leave the index frozen
/// behind the chain; once no head arrives for half the report's life, the
/// session ends, and the next one catches up from the cursor.
#[tokio::test]
async fn a_silent_socket_is_left_and_the_next_session_catches_up() {
    let Some(mut chain) = Chain::start(43126).await else {
        return;
    };
    let running = chain.run(IndexerConfig {
        stale_after: Duration::from_secs(2),
        poll_interval: Duration::from_millis(500),
        ..chain.config(2)
    });
    chain.wait_for_index(chain.head().await - 2).await;

    // The quiet session ends and the next one opens; neither is a failure
    // a monitor should see.
    chain.relay.silence(true);
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while chain.called("eth_subscribe") < 2 {
        assert_eq!(chain.window_error().await, None);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the silent session was never left"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let holder = chain.accounts()[0];
    let bound = chain.bind("silenced_1", holder).await;
    chain.mine(3).await;
    chain.wait_for_index(bound).await;

    assert_eq!(chain.resolve("silenced_1").await, Ok(holder));
    assert!(
        chain.called("eth_subscribe") >= 2,
        "{:?}",
        chain.relay.calls()
    );
    chain.stop(running).await;
}
