//! The subscription source against a real chain. anvil streams the mock
//! contracts' logs over a WebSocket, through a relay that can drop the socket
//! and records every call the indexer makes, so each test checks what the
//! indexer committed and what the RPC was asked for it.
//!
//! Skips silently in exactly two cases: `DATABASE_URL` unset, or no `anvil`
//! binary on PATH. Everything past those checks panics on failure.

use std::time::Duration;

use alloy::{
    node_bindings::{
        Anvil,
        AnvilInstance,
    },
    primitives::{
        address,
        Address,
        U256,
    },
    providers::{
        ext::AnvilApi,
        DynProvider,
        Provider,
        ProviderBuilder,
    },
    rpc::types::{
        anvil::{
            ReorgOptions,
            TransactionData,
        },
        TransactionRequest,
    },
};
use axum::http::StatusCode;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::HandleResolution,
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
    nodes::{
        self,
        NormalizedHandle,
        Platform,
    },
};

mod common;
mod mocks;
mod relay;
use mocks::{
    MockHandleEscrow::{
        self,
        MockHandleEscrowInstance,
    },
    MockIdentityRegistry::{
        self,
        MockIdentityRegistryInstance,
    },
};
use relay::Relay;

/// How long any wait on the indexer may take before the test fails.
const PATIENCE: Duration = Duration::from_secs(30);

/// One test's chain: anvil with both mocks deployed, the relay the indexer
/// dials, and the store it writes under the chain's writer lease. Reads
/// without a chain span every suite's rows, so the handles and addresses
/// this suite binds are its own.
struct Chain {
    anvil: AnvilInstance,
    provider: DynProvider,
    registry: MockIdentityRegistryInstance<DynProvider>,
    escrow: MockHandleEscrowInstance<DynProvider>,
    relay: Relay,
    store: ChainStore,
    writer: WriterLease,
}

/// The indexer task a test runs, and the token that stops it.
struct Running {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Chain {
    /// A fresh chain with id `chain_id` and its rows cleared, or `None` where
    /// the suite skips.
    async fn start(chain_id: u64) -> Option<Self> {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        };
        if std::process::Command::new("anvil")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: anvil not on PATH");
            return None;
        }

        let pool = db::connect_and_migrate(&url).await.expect("database");
        for table in db::PROJECTION_TABLES {
            sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
                .bind(chain_id as i64)
                .execute(&pool)
                .await
                .expect("cleanup");
        }
        sqlx::query("DELETE FROM names.chain_metadata WHERE chain_id = $1")
            .bind(chain_id as i64)
            .execute(&pool)
            .await
            .expect("metadata cleanup");
        let store = ChainStore::new(pool, chain_id as i64);

        let anvil = Anvil::new().chain_id(chain_id).spawn();
        let provider = ProviderBuilder::new()
            .wallet(anvil.wallet().expect("anvil's dev accounts"))
            .connect_http(anvil.endpoint_url())
            .erased();
        let registry = MockIdentityRegistry::deploy(provider.clone())
            .await
            .expect("deploy the registry");
        let escrow = MockHandleEscrow::deploy(provider.clone(), *registry.address())
            .await
            .expect("deploy the escrow");
        // A few blocks of history, so the catch-up spans more than a window.
        provider.anvil_mine(Some(8), None).await.expect("mine");
        let relay = Relay::start(anvil.ws_endpoint_url()).await;

        let writer = store.acquire_writer().await.expect("writer lease");
        store
            .prepare(&writer, *registry.address(), Some(*escrow.address()))
            .await
            .expect("prepare");
        Some(Self {
            anvil,
            provider,
            registry,
            escrow,
            relay,
            store,
            writer,
        })
    }

    /// Run the indexer over the relay with a log subscription, staying
    /// `confirmations` behind the head.
    fn index(&self, confirmations: u64) -> Running {
        let config = IndexerConfig {
            contract: *self.registry.address(),
            escrow: Some(*self.escrow.address()),
            confirmations,
            source: LogSource::Subscribe,
            poll_interval: Duration::from_millis(100),
            head_interval: Duration::from_millis(300),
            max_block_range: 5,
            start_block: Some(0),
            stale_after: Duration::from_secs(120),
        };
        let cancel = CancellationToken::new();
        let indexer = Indexer::new(self.store.clone(), self.relay.url(), config);
        let task = tokio::spawn(indexer.run(cancel.clone()));
        Running { cancel, task }
    }

    /// The chain head.
    async fn head(&self) -> u64 {
        self.provider.get_block_number().await.expect("head")
    }

    /// Mine `blocks` empty blocks.
    async fn mine(&self, blocks: u64) {
        self.provider
            .anvil_mine(Some(blocks), None)
            .await
            .expect("mine");
    }

    /// The transaction that binds `handle` on X to `holder`, unsent.
    fn binding(&self, handle: &str, holder: Address) -> TransactionRequest {
        let x = Platform::from_key("x").expect("x is a platform").id();
        self.registry
            .emitIdentityBound(
                holder,
                nodes::id_node(x, handle),
                nodes::handle_node(x, &NormalizedHandle::from_chain(handle)),
                x,
                handle.into(),
                handle.into(),
                1000,
                true,
                1,
            )
            .into_transaction_request()
            .from(self.anvil.addresses()[0])
    }

    /// Bind `handle` on X to `holder`; the block the binding landed in.
    async fn bind(&self, handle: &str, holder: Address) -> u64 {
        self.provider
            .send_transaction(self.binding(handle, holder))
            .await
            .expect("send the binding")
            .get_receipt()
            .await
            .expect("the binding's receipt")
            .block_number
            .expect("a mined binding")
    }

    /// Escrow a native deposit for `handle` on X, which nobody holds yet;
    /// the block it landed in.
    async fn deposit(&self, handle: &str, payer: Address) -> u64 {
        let x = Platform::from_key("x").expect("x is a platform").id();
        let native = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
        self.escrow
            .emitDeposited(
                nodes::handle_node(x, &NormalizedHandle::from_chain(handle)),
                native,
                payer,
                payer,
                x,
                U256::ZERO,
                U256::from(100u64),
            )
            .send()
            .await
            .expect("send the deposit")
            .get_receipt()
            .await
            .expect("the deposit's receipt")
            .block_number
            .expect("a mined deposit")
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

    /// Who `handle` on X resolves to, or the refusal's status.
    async fn resolve(&self, handle: &str) -> Result<Address, StatusCode> {
        let chain = self.store.chain_id();
        let reply = common::get::<HandleResolution>(
            &self.store,
            &format!("/v1/resolve/handle/x/{handle}?chain={chain}"),
        )
        .await;
        if reply.status.is_success() {
            Ok(reply.answer().bindings[0].owner)
        } else {
            Err(reply.refusal().0)
        }
    }

    /// The journal's event kinds by block, in chain order.
    async fn journal(&self) -> Vec<(i64, String)> {
        sqlx::query_as(
            "SELECT block_number, kind FROM names.events WHERE chain_id = $1 \
             ORDER BY block_number, log_index",
        )
        .bind(self.store.chain_id())
        .fetch_all(self.store.pool())
        .await
        .expect("journal")
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
    let running = chain.index(2);
    chain.wait_for_index(chain.head().await - 2).await;
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
        position.valid_for.is_some_and(|valid_for| valid_for > 0),
        "{position:?}"
    );
    chain.stop(running).await;
}

/// A pushed event commits once its block is confirmed, both watched
/// contracts' alike, and costs one canonical block read per block holding
/// events rather than any window.
#[tokio::test]
async fn pushed_events_commit_once_confirmed_at_one_block_read_each() {
    let Some(chain) = Chain::start(43121).await else {
        return;
    };
    let running = chain.index(2);
    chain.wait_for_index(chain.head().await - 2).await;
    let windows = chain.called("eth_getLogs");

    let holder = Address::repeat_byte(0x51);
    let bound = chain.bind("pushed_1", holder).await;
    let deposited = chain
        .deposit("deposited_1", Address::repeat_byte(0x52))
        .await;
    chain.mine(2).await;
    chain.wait_for_index(deposited).await;

    assert_eq!(chain.resolve("pushed_1").await, Ok(holder));
    assert_eq!(
        chain.journal().await,
        [
            (bound as i64, "identity_bound".to_string()),
            (deposited as i64, "deposited".to_string()),
        ]
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
/// fetches them from the cursor, once.
#[tokio::test]
async fn a_dropped_socket_is_backfilled_from_the_cursor_on_reconnect() {
    let Some(chain) = Chain::start(43122).await else {
        return;
    };
    let running = chain.index(2);
    chain.wait_for_index(chain.head().await - 2).await;

    // The stream is gone before the event is emitted, so only a backfill
    // can find it.
    chain.relay.cut().await;
    let holder = Address::repeat_byte(0x53);
    let bound = chain.bind("backfilled_1", holder).await;
    chain.mine(3).await;
    chain.relay.reopen();
    chain.wait_for_index(bound).await;

    assert_eq!(chain.resolve("backfilled_1").await, Ok(holder));
    assert_eq!(
        chain.journal().await,
        [(bound as i64, "identity_bound".to_string())]
    );
    assert_eq!(
        chain.called("eth_subscribe"),
        2,
        "{:?}",
        chain.relay.calls()
    );
    chain.stop(running).await;
}

/// A reorg that replaces the blocks above the confirmation depth drops the
/// event a replaced block held, and keeps the one its replacement holds: the
/// canonical block at each height decides, read once that height is
/// confirmed.
#[tokio::test]
async fn a_reorg_at_the_confirmation_depth_drops_the_event_it_orphaned() {
    let Some(chain) = Chain::start(43123).await else {
        return;
    };
    let running = chain.index(3);
    chain.wait_for_index(chain.head().await - 3).await;
    let windows = chain.called("eth_getLogs");

    let (orphan, heir) = (Address::repeat_byte(0x54), Address::repeat_byte(0x55));
    let orphaned = chain.bind("orphaned_1", orphan).await;
    chain.mine(2).await;
    // Pushed and held, but two blocks deep: not yet confirmed.
    let cursor = chain.store.cursor().await.expect("cursor");
    assert!(cursor.is_some_and(|cursor| cursor < orphaned), "{cursor:?}");
    chain
        .provider
        .anvil_reorg(ReorgOptions {
            depth: 3,
            tx_block_pairs: vec![(
                TransactionData::JSON(chain.binding("replacing_1", heir)),
                1,
            )],
        })
        .await
        .expect("reorg");
    chain.mine(2).await;
    chain.wait_for_index(orphaned + 1).await;

    assert_eq!(chain.resolve("replacing_1").await, Ok(heir));
    assert_eq!(
        chain.resolve("orphaned_1").await,
        Err(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        chain.journal().await,
        [((orphaned + 1) as i64, "identity_bound".to_string())]
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
