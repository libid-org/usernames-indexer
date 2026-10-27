//! The subscription source against a real chain. anvil pushes the
//! contracts' logs over a WebSocket, through a relay that can drop the
//! socket, deafen it, hold answers, and delay or retract pushed logs, and
//! that records every call the indexer makes, so each test checks what the
//! indexer committed and what the RPC was asked for it.
//!
//! Skips without `DATABASE_URL` or `anvil`, unless `CI` is set: there a
//! missing one fails. Everything past those checks panics on failure.

use std::{
    collections::BTreeSet,
    time::Duration,
};

use alloy::{
    primitives::{
        Address,
        U256,
    },
    providers::{
        ext::AnvilApi,
        Provider,
    },
    rpc::types::Filter,
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
        Superseded,
        WriterLease,
    },
    indexer::{
        Indexer,
        IndexerConfig,
        LogSource,
    },
    nodes::KnownPlatform,
};

mod relay;
use relay::Relay;
use usernames_fixtures::{
    anvil_available,
    database_url,
    libid::{
        Disclose,
        Handle,
        Libid,
        Node,
        Proof,
    },
    ChainWatch,
    Reads,
    Seed,
    PATIENCE,
};

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
    task: JoinHandle<Result<(), Superseded>>,
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
        let Some(url) = database_url() else {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        };
        if !anvil_available() {
            eprintln!("skipping: anvil not on PATH");
            return None;
        }

        let pool = db::connect_and_migrate(&url).await.expect("database");
        ChainStore::new(pool.clone(), chain_id as i64)
            .forget()
            .await;
        let store = ChainStore::new(pool.clone(), chain_id as i64);

        let node = Node::spawn(chain_id);
        let libid = Libid::deploy(&node.provider, node.accounts()[0]).await;
        // A few blocks of history, so the catch-up spans more than a window.
        node.provider.anvil_mine(Some(8), None).await.expect("mine");
        let relay = Relay::start(node.ws()).await;

        let lease = store.acquire_writer().await.expect("writer lease");
        let writer = store
            .prepare(&lease, libid.contracts())
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

    /// The suite's indexer settings: a log subscription over the relay,
    /// `confirmations` behind the head, on short intervals.
    fn config(&self, confirmations: u64) -> IndexerConfig {
        IndexerConfig {
            contracts: self.libid.contracts(),
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

    /// Wait until the index stands at `block` or past it: the cursor
    /// committed through it, and the target reported beside it.
    async fn wait_for_index(&self, block: u64) {
        let reached = |at: Option<u64>| at.is_some_and(|at| at >= block);
        ChainWatch::new(&self.pool, self.writer.store())
            .await
            .until(|state| {
                let position = state.position();
                reached(position.cursor) && reached(position.target)
            })
            .await;
    }

    /// Who `handle` on X resolves to, or the refusal's status.
    async fn resolve(&self, handle: &str) -> Result<Address, StatusCode> {
        let reply = self
            .pool
            .get_on::<HandleResolution>(
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
        let history: HandleHistory = self
            .pool
            .get_on(
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
        running
            .task
            .await
            .expect("the indexer task")
            .expect("the indexer stops cleanly");
        self.lease.release().await.expect("the lease releases");
    }
}

/// A pushed event commits once its block is confirmed, both watched
/// contracts' alike, and no block past the head the session first read is
/// fetched: the pushed logs are all the loop commits by.
#[tokio::test]
async fn pushed_events_commit_once_confirmed_with_no_fetch() {
    let Some(mut chain) = Chain::start(43121).await else {
        return;
    };
    let running = chain.run(chain.config(2));
    // Past the session's first head read, every block it covers is indexed.
    chain.relay.called("eth_blockNumber", 1).await;
    let first = chain.head().await;
    chain.mine(2).await;
    chain.wait_for_index(first).await;
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

    chain.mine(5).await;
    chain.wait_for_index(chain.head().await - 2).await;
    assert_eq!(
        chain.called("eth_getLogs"),
        windows,
        "{:?}",
        chain.relay.calls()
    );
    // The endpoint pushes the logs' block times: no header is asked for.
    assert_eq!(chain.called("eth_getBlockByHash"), 0);
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
    // Logs, once per session.
    assert_eq!(
        chain.called("eth_subscribe"),
        2,
        "{:?}",
        chain.relay.calls()
    );
    chain.stop(running).await;
}

/// A head that stands still for half the report's life ends the session,
/// and the next one reconnects: a backend that stopped following the chain
/// is left. Neither is a failure a monitor should see.
#[tokio::test]
async fn a_still_head_ends_the_session_and_the_next_one_reconnects() {
    let Some(mut chain) = Chain::start(43126).await else {
        return;
    };
    let running = chain.run(IndexerConfig {
        stale_after: Duration::from_secs(2),
        ..chain.config(2)
    });
    chain.wait_for_index(chain.head().await - 2).await;

    // Nothing is mined, so the head stands still.
    let mut watch = ChainWatch::new(&chain.pool, chain.writer.store()).await;
    let stored = async {
        loop {
            if let Some(error) = watch.next().await.window_error {
                return error;
            }
        }
    };
    tokio::time::timeout(PATIENCE, async {
        tokio::select! {
            () = chain.relay.called("eth_subscribe", 2) => {}
            error = stored => panic!("a still head stored a window error: {error}"),
        }
    })
    .await
    .expect("the session on a still head was left");

    let holder = chain.accounts()[0];
    let bound = chain.bind("still_1", holder).await;
    chain.mine(3).await;
    chain.wait_for_index(bound).await;
    assert_eq!(chain.resolve("still_1").await, Ok(holder));
    chain.stop(running).await;
}

/// A log a reorg takes back before its block is committed is never
/// committed: the removal drops the pushed log it names.
#[tokio::test]
async fn a_log_a_reorg_removed_is_never_committed() {
    let Some(mut chain) = Chain::start(43142).await else {
        return;
    };
    let running = chain.run(chain.config(2));
    chain.wait_for_index(chain.head().await - 2).await;

    chain.relay.retract(true);
    let holder = chain.accounts()[0];
    let retracted = chain.bind("retracted_1", holder).await;
    chain.mine(2).await;
    chain.wait_for_index(retracted).await;
    assert_eq!(
        chain.resolve("retracted_1").await,
        Err(StatusCode::NOT_FOUND)
    );

    chain.relay.retract(false);
    let kept = chain.bind("kept_1", holder).await;
    chain.mine(2).await;
    chain.wait_for_index(kept).await;
    assert_eq!(chain.resolve("kept_1").await, Ok(holder));
    chain.stop(running).await;
}

/// The subscription opens before the catch-up: a log emitted while the
/// catch-up's `eth_getLogs` is still out, in a block past it, is pushed and
/// committed once.
#[tokio::test]
async fn a_log_emitted_during_the_catch_up_is_pushed_and_committed() {
    let Some(mut chain) = Chain::start(43143).await else {
        return;
    };
    chain.relay.hold(Some("eth_getLogs"));
    let running = chain.run(chain.config(2));
    chain.relay.called("eth_getLogs", 1).await;

    let holder = chain.accounts()[0];
    let bound = chain.bind("midway_1", holder).await;
    chain.mine(2).await;
    chain.relay.hold(None);
    chain.wait_for_index(bound).await;

    assert_eq!(chain.resolve("midway_1").await, Ok(holder));
    let history = chain.history("midway_1").await;
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
    chain.stop(running).await;
}

/// A log mined before the subscription opened is never pushed, and the
/// catch-up stops `CONFIRMATIONS` short of the head it read: the blocks in
/// between are fetched once confirmed.
#[tokio::test]
async fn a_log_mined_before_the_subscription_past_the_catch_up_lands() {
    let Some(mut chain) = Chain::start(43145).await else {
        return;
    };
    let holder = chain.accounts()[0];
    let bound = chain.bind("unpushed_1", holder).await;
    let running = chain.run(chain.config(2));
    chain.wait_for_index(bound - 2).await;

    chain.mine(2).await;
    chain.wait_for_index(bound).await;
    assert_eq!(chain.resolve("unpushed_1").await, Ok(holder));
    chain.stop(running).await;
}

/// A tick commits through the target the tick before it read, so a log a
/// lagging stream pushes after the head moved past its block still lands.
/// Each bind here lands shortly before a tick and its log arrives shortly
/// after that tick, which reads the bind's block as the head.
#[tokio::test]
async fn a_log_a_lagging_stream_pushes_after_the_head_passed_it_lands() {
    let Some(mut chain) = Chain::start(43144).await else {
        return;
    };
    let interval = Duration::from_secs(1);
    chain.relay.delay_pushes(interval * 4 / 5);
    let running = chain.run(IndexerConfig {
        poll_interval: interval,
        ..chain.config(0)
    });
    chain.wait_for_index(chain.head().await).await;

    let holder = chain.accounts()[0];
    for handle in ["lagging_1", "lagging_2"] {
        // A tick writes the chain's row; the next comes an interval later.
        ChainWatch::new(&chain.pool, chain.writer.store())
            .await
            .next()
            .await;
        tokio::time::sleep(interval * 3 / 5).await;
        let bound = chain.bind(handle, holder).await;
        chain.wait_for_index(bound).await;
        assert_eq!(chain.resolve(handle).await, Ok(holder), "{handle}");
    }
    chain.stop(running).await;
}

/// An endpoint that keeps its socket open but stops answering would hold a
/// session forever; a call left unanswered for the loop's patience fails,
/// the failure is stored where a monitor looks, and once the endpoint
/// answers again the next session catches up from the cursor.
#[tokio::test]
async fn a_deaf_endpoint_is_left_and_the_next_session_catches_up() {
    let Some(mut chain) = Chain::start(43141).await else {
        return;
    };
    let running = chain.run(IndexerConfig {
        stale_after: Duration::from_secs(2),
        ..chain.config(2)
    });
    chain.wait_for_index(chain.head().await - 2).await;

    let mut watch = ChainWatch::new(&chain.pool, chain.writer.store()).await;
    // The next tick's head read goes unanswered.
    chain.relay.deafen(true);
    let stored = tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(error) = watch.next().await.window_error {
                return error;
            }
        }
    })
    .await
    .expect("the unanswered call failed and its error was stored");
    assert_eq!(stored.matches("no answer within").count(), 1, "{stored}");

    chain.relay.deafen(false);
    let holder = chain.accounts()[0];
    let bound = chain.bind("deafened_1", holder).await;
    chain.mine(3).await;
    chain.wait_for_index(bound).await;

    assert_eq!(chain.resolve("deafened_1").await, Ok(holder));
    chain.stop(running).await;
}

/// An indexer whose chain another instance took stops at its next window,
/// rather than retrying a chain that is no longer its to write.
#[tokio::test]
async fn an_indexer_whose_chain_was_taken_stops() {
    let Some(chain) = Chain::start(43127).await else {
        return;
    };
    let running = chain.run(chain.config(1));
    chain.wait_for_index(chain.head().await - 1).await;

    // Another instance takes the chain: this lease's session ends, and the
    // next holder's prepare names a new writer.
    let store = chain.writer.store();
    sqlx::query(include_str!("sql/end_writer_session.sql"))
        .bind(store.chain_id())
        .execute(&chain.pool)
        .await
        .expect("the lease's session ends");
    store.writer().await;
    chain.mine(2).await;

    let stopped = tokio::time::timeout(PATIENCE, running.task)
        .await
        .expect("the indexer stops")
        .expect("the indexer task");
    assert!(
        matches!(stopped, Err(Superseded { chain_id }) if chain_id == store.chain_id()),
        "{stopped:?}"
    );
}

/// An endpoint whose logs leave out `blockTimestamp`, answered by
/// `eth_getLogs` and pushed alike, still has every event indexed, at its
/// block's time.
#[tokio::test]
async fn logs_without_block_times_take_their_blocks_time() {
    let Some(mut chain) = Chain::start(43131).await else {
        return;
    };
    chain.relay.strip_block_times(true);
    let running = chain.run(chain.config(1));
    let holder = chain.accounts()[0];
    let bound = chain.bind("timeless_1", holder).await;
    chain.mine(2).await;
    chain.wait_for_index(bound).await;

    let history: HandleHistory = chain
        .pool
        .get_on(
            chain.writer.store().chain_id(),
            "/v1/history/handle/x/timeless_1",
        )
        .await
        .answer();
    let block = chain
        .node
        .provider
        .get_block_by_number(bound.into())
        .await
        .expect("the block")
        .expect("a mined block");
    let times: Vec<i64> = history
        .entries
        .iter()
        .map(|entry| entry.block_time)
        .collect();
    assert!(!times.is_empty());
    // One header per block that holds a watched log, the contracts'
    // deployments included.
    let filter = Filter::new()
        .address(chain.libid.contracts().watched())
        .from_block(0)
        .to_block(bound);
    let logged: BTreeSet<_> = chain
        .node
        .provider
        .get_logs(&filter)
        .await
        .expect("the logs")
        .into_iter()
        .filter_map(|log| log.block_hash)
        .collect();
    assert_eq!(chain.called("eth_getBlockByHash"), logged.len());
    assert!(
        times
            .iter()
            .all(|time| *time == block.header.timestamp as i64),
        "{times:?} against {}",
        block.header.timestamp
    );
    chain.stop(running).await;
}
