//! The indexer binary as a supervisor runs it: stopped by SIGTERM while it
//! waits for its chain's lease, and while it indexes.

#[path = "../../../crates/usernames-core/tests/common/mod.rs"]
mod common;

use std::{
    process::{
        ExitStatus,
        Stdio,
    },
    time::Duration,
};

use common::{
    libid::{
        Libid,
        Node,
    },
    ChainWatch,
    PATIENCE,
};
use sqlx::PgPool;
use tokio::{
    io::{
        AsyncBufReadExt,
        BufReader,
    },
    process::{
        Child,
        Command,
    },
    sync::watch,
};
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

/// An indexer process, killed if the test ends before it does, and what it
/// logged so far.
struct Running {
    child: Child,
    log: watch::Receiver<Vec<String>>,
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
        ChainStore::new(pool.clone(), chain as i64).forget().await;
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
    fn indexer(&self) -> Running {
        let mut child = Command::new(env!("CARGO_BIN_EXE_usernames-indexer"))
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
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the indexer starts");
        let mut lines = BufReader::new(child.stdout.take().expect("its log")).lines();
        let (logged, log) = watch::channel(Vec::new());
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                logged.send_modify(|log| log.push(line));
            }
        });
        Running { child, log }
    }
}

impl Running {
    /// Return once the indexer has logged a line holding `text`.
    async fn logged(&mut self, text: &str) {
        tokio::time::timeout(
            PATIENCE,
            self.log
                .wait_for(|log| log.iter().any(|line| line.contains(text))),
        )
        .await
        .expect("logged in time")
        .map(drop)
        .unwrap_or_else(|_| panic!("the indexer exited without logging {text:?}"));
    }

    /// SIGTERM the indexer, as a supervisor stops it, and its exit status.
    async fn terminate(mut self) -> ExitStatus {
        let pid = self.child.id().expect("a running indexer");
        let sent = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await
            .expect("kill runs");
        assert!(sent.success());
        tokio::time::timeout(EXIT_WITHIN, self.child.wait())
            .await
            .expect("the indexer stops in time")
            .expect("its exit status")
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
    indexer
        .logged("another indexer holds this chain's writer lock; waiting")
        .await;

    assert!(indexer.terminate().await.success());
    held.release().await.expect("the lease goes back");
}

/// Stopped while it indexes, the indexer finishes its window, gives the
/// chain's lease back and exits cleanly.
#[tokio::test]
async fn an_indexing_indexer_stops_on_sigterm_and_frees_the_chain() {
    let Some(chain) = Chain::start(43130).await else {
        return;
    };
    let mut watch = ChainWatch::new(&chain.pool, &chain.store).await;
    let indexer = chain.indexer();
    watch.cursor_at(1).await;

    assert!(indexer.terminate().await.success());
    let next = tokio::time::timeout(Duration::from_secs(1), chain.store.acquire_writer())
        .await
        .expect("the chain's lease is free")
        .expect("the lease");
    next.release().await.expect("the lease goes back");
}
