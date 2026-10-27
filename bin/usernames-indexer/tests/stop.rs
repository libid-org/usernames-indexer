//! The indexer binary as a supervisor runs it: stopped by SIGTERM while it
//! waits for its chain's lease, and while it indexes.

#[path = "../../../crates/usernames-core/tests/common/mod.rs"]
mod common;

use std::{
    process::{
        Child,
        Command,
        ExitStatus,
    },
    time::Duration,
};

use common::libid::{
    Libid,
    Node,
};
use sqlx::PgPool;
use tokio::time::Instant;
use usernames_core::db::{
    self,
    ChainStore,
};

/// How long a stopped indexer may take to exit: one lease check, and the
/// release after it.
const EXIT_WITHIN: Duration = Duration::from_secs(12);

/// A chain with the libid contracts deployed, and its emptied store.
struct Chain {
    node: Node,
    libid: Libid,
    pool: PgPool,
    store: ChainStore,
    database: String,
}

impl Chain {
    /// `chain` on a fresh anvil; `None` when the suite skips.
    async fn start(chain: u64) -> Option<Self> {
        let Some(database) = common::database_url() else {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        };
        if !common::anvil_available() {
            eprintln!("skipping: anvil not on PATH");
            return None;
        }
        let pool = db::connect_and_migrate(&database).await.expect("database");
        common::clear_chain(&pool, chain as i64).await;
        let node = Node::spawn(chain);
        let libid = Libid::deploy(&node.provider, node.accounts()[0]).await;
        Some(Self {
            node,
            libid,
            store: ChainStore::new(pool.clone(), chain as i64),
            pool,
            database,
        })
    }

    /// The indexer binary for this chain, polling.
    fn indexer(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_usernames-indexer"))
            .env("DATABASE_URL", &self.database)
            .env("RPC_URL", self.node.rpc().as_str())
            .env("LOG_SOURCE", "poll")
            .env(
                "IDENTITY_NAMES_ADDRESS",
                self.libid.registry.address().to_string(),
            )
            .env(
                "HANDLE_ESCROW_ADDRESS",
                self.libid.escrow.address().to_string(),
            )
            .env("CHAIN_NAMES", format!("stop-{}", self.store.chain_id()))
            .env("CONFIRMATIONS", "0")
            .env("POLL_INTERVAL", "100ms")
            .env("START_BLOCK", "0")
            .spawn()
            .expect("the indexer starts")
    }
}

/// SIGTERM `child`, as a supervisor stops it, and its exit status.
async fn terminate(child: &mut Child) -> ExitStatus {
    let sent = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(sent.success());
    let deadline = Instant::now() + EXIT_WITHIN;
    loop {
        if let Some(status) = child.try_wait().expect("the indexer's status") {
            return status;
        }
        assert!(Instant::now() < deadline, "the indexer did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// During a rolling deploy the new indexer waits for the old one's lease;
/// stopped then, it exits at once instead of at the supervisor's kill.
#[tokio::test]
async fn an_indexer_waiting_for_the_lease_stops_on_sigterm() {
    let Some(chain) = Chain::start(43129).await else {
        return;
    };
    let held = chain.store.acquire_writer().await.expect("the lease");
    let mut indexer = chain.indexer();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !sqlx::query_scalar::<_, bool>(include_str!("sql/waiting_for_the_lease.sql"))
        .fetch_one(&chain.pool)
        .await
        .expect("the waits")
    {
        assert!(
            Instant::now() < deadline,
            "the indexer never waited for the lease"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(terminate(&mut indexer).await.success());
    held.release().await.expect("the lease goes back");
}

/// Stopped while it indexes, the indexer finishes its window, gives the
/// chain's lease back and exits cleanly.
#[tokio::test]
async fn an_indexing_indexer_stops_on_sigterm_and_frees_the_chain() {
    let Some(chain) = Chain::start(43130).await else {
        return;
    };
    let mut indexer = chain.indexer();
    common::wait_for_cursor(&chain.store, 1).await;

    assert!(terminate(&mut indexer).await.success());
    let next = tokio::time::timeout(Duration::from_secs(1), chain.store.acquire_writer())
        .await
        .expect("the chain's lease is free")
        .expect("the lease");
    next.release().await.expect("the lease goes back");
}
