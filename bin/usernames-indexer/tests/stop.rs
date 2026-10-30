//! The indexer binary as a supervisor runs it: stopped by SIGTERM while it
//! waits for its chain's lease, and while it indexes; refused at start.

use std::{
    process::{
        ExitStatus,
        Stdio,
    },
    time::Duration,
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
use usernames_core::{
    db::{
        self,
        ChainStore,
    },
    ens::ChainName,
};
use usernames_fixtures::{
    anvil_available,
    database_url,
    libid::{
        Libid,
        Node,
    },
    ChainWatch,
    PATIENCE,
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
        let Some(database) = database_url() else {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        };
        if !anvil_available() {
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

    /// The indexer binary for this chain, polling, not yet started.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_usernames-indexer"));
        command
            .env("DATABASE_URL", &self.database)
            .env("RPC_URL", self.node.rpc().as_str())
            .env(
                "IDENTITY_REGISTRY_ADDRESS",
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
            .kill_on_drop(true);
        command
    }

    /// The indexer binary for this chain, running.
    fn indexer(&self) -> Running {
        let mut child = self
            .command()
            .stdout(Stdio::piped())
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
    let indexer = chain.indexer();
    chain.store.cursor_at(1).await;

    assert!(indexer.terminate().await.success());
    let next = tokio::time::timeout(Duration::from_secs(1), chain.store.acquire_writer())
        .await
        .expect("the chain's lease is free")
        .expect("the lease");
    next.release().await.expect("the lease goes back");
}

/// A start that cannot declare its chain's names refuses before it clears
/// anything: a name another chain holds stops the process and leaves the
/// chain's rows as they were, even where the start would replay the chain.
#[tokio::test]
async fn a_start_refused_for_a_taken_name_keeps_the_chain() {
    let Some(chain) = Chain::start(43161).await else {
        return;
    };
    let indexer = chain.indexer();
    chain.store.cursor_at(1).await;
    assert!(indexer.terminate().await.success());

    let other = ChainStore::new(chain.pool.clone(), 43162);
    other.forget().await;
    let mut lease = other
        .acquire_writer()
        .await
        .expect("the other chain's lease");
    let taken: ChainName = "taken".parse().expect("a label");
    other
        .set_chain_names(&mut lease, &[taken])
        .await
        .expect("the other chain holds the name");

    // Without its escrow, the start would replay the chain.
    let refused = chain
        .command()
        .env("CHAIN_NAMES", "taken")
        .env_remove("HANDLE_ESCROW_ADDRESS")
        .output()
        .await
        .expect("the indexer runs");
    assert!(!refused.status.success());
    let shown = String::from_utf8_lossy(&refused.stderr);
    assert!(shown.contains("already belongs to chain 43162"), "{shown}");
    assert!(
        chain.store.cursor().await.expect("the cursor").is_some(),
        "the chain's rows are kept"
    );

    lease.release().await.expect("the lease goes back");
    other.forget().await;
}
