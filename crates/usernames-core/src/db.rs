//! Postgres, behind types that carry the invariants.
//!
//! [`ChainStore`] fuses the pool with the chain it scopes every query to, so
//! no call site can pass the wrong chain id or forget it. [`ChainWriter`] is
//! the right to write one chain, which only a prepare under the writer lease
//! hands out. [`Window`] is an open window: applying events and
//! committing-with-cursor are its only operations, which makes "the cursor
//! and the writes it stands for commit together or not at all" a fact of the
//! type rather than caller discipline.
//! The journal's `(chain, block, log)` conflict gates every write in
//! [`Window::apply`], so replaying a window — after a crash, or after a
//! version-bump re-index — converges instead of counting an event twice.

use std::time::Duration;

use alloy::primitives::{
    Address,
    B256,
    U256,
};
use chrono::TimeDelta;
use sqlx::{
    migrate::Migrator,
    Connection,
    PgPool,
    Postgres,
    QueryBuilder,
    Transaction,
};

use crate::{
    ens::ChainName,
    events::{
        LogPosition,
        NamesEvent,
    },
};

mod escrow;
mod history;
mod journal;
mod page;

pub use self::{
    escrow::{
        ContributionRow,
        EscrowSlotRow,
        SlotCursor,
        UnclaimedCursor,
    },
    history::{
        HistoryCursor,
        HistoryRow,
    },
    journal::{
        EventColumns,
        JournalError,
    },
    page::{
        Cursor,
        InvalidCursor,
        Page,
        Rows,
    },
};

/// Bump on any change to what the indexer writes. A mismatch at startup
/// clears the chain's rows and cursor, so the next loop replays the chain
/// from the deployment block — the re-index IS the migration.
pub const INDEXER_VERSION: &str = "1";

/// One chain's row: what its indexer watches and writes, how far it got,
/// and its last report. Blocks and times are what the indexer wrote, so
/// never negative.
#[derive(Clone, Debug, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct ChainState {
    /// The chain.
    pub chain_id: i64,
    /// The [`INDEXER_VERSION`] its rows were written by.
    pub indexer_version: Option<String>,
    /// The registry its rows come from.
    pub contract: Option<Address>,
    /// The escrow its escrow rows come from, when it watches one.
    pub escrow: Option<Address>,
    /// The registry's deployment block, once detected.
    pub deploy_block: Option<i64>,
    /// The last block a window committed through.
    pub cursor: Option<i64>,
    /// How far the chain had grown at the indexer's last look.
    pub head: Option<i64>,
    /// The block the indexer last set out to reach.
    pub target: Option<i64>,
    /// Unix seconds at which the indexer last reported.
    pub reported_at: Option<i64>,
    /// Seconds until that report expires; negative once it has.
    pub valid_for: Option<i64>,
    /// The last window failure, while the most recent window failed.
    pub window_error: Option<String>,
    /// The Proof Verifier the registry was last pointed at.
    pub proof_verifier: Option<Address>,
}

impl ChainState {
    /// How far the chain had grown at the indexer's last look.
    pub fn head(&self) -> Option<u64> {
        block(self.head)
    }

    /// The registry's deployment block as stored, while the row is about
    /// `contract`.
    pub fn deploy_block_of(&self, contract: Address) -> Option<i64> {
        self.deploy_block
            .filter(|_| self.contract == Some(contract))
    }

    /// Where the index stands.
    pub fn position(&self) -> IndexPosition {
        IndexPosition {
            target: block(self.target),
            cursor: block(self.cursor),
            reported_at: block(self.reported_at),
            valid_for: self.valid_for.and_then(TimeDelta::try_seconds),
        }
    }
}

/// A stored block number or time, which the indexer wrote from a `u64`.
fn block(value: Option<i64>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

/// What the ENS gateway answers from: where the index stands, and who holds
/// the node asked about, read together.
#[derive(Clone, Copy, Debug)]
pub struct EnsAnswer {
    /// Where the index stands; `None` before an indexer prepared the chain.
    pub position: Option<IndexPosition>,
    /// Who holds the node; `None` when nobody does.
    pub holder: Option<Address>,
}

/// The chain's row and the holder, as `ens_answer.sql` reads them.
#[derive(sqlx::FromRow)]
struct EnsRow {
    #[sqlx(flatten)]
    state: ChainState,
    owner: Option<Address>,
}

/// A chain's row and the names it goes by: one entry of `/v1/status`.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ChainStatusRow {
    /// The chain's row.
    #[sqlx(flatten)]
    pub state: ChainState,
    /// The names its indexer declared.
    pub names: Vec<String>,
}

/// The index's standing, as [`ChainState::position`] reads it, every moment
/// by the database's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexPosition {
    /// The block the indexer last set out to reach.
    pub target: Option<u64>,
    /// The block the cursor has committed through.
    pub cursor: Option<u64>,
    /// Unix seconds at which the indexer last reported.
    pub reported_at: Option<u64>,
    /// How long until the indexer's last report expires; negative once it
    /// has.
    pub valid_for: Option<TimeDelta>,
}

impl IndexPosition {
    /// Whole seconds until the last report expires, negative once it has.
    pub fn valid_for_seconds(&self) -> Option<i64> {
        self.valid_for.map(|valid_for| valid_for.num_seconds())
    }
}

/// The channel every change to a chain's row is announced on, with the
/// chain's id as the payload, once the change commits.
pub const CHAIN_CHANNEL: &str = "names_chains";

/// The longest a report may be trusted: a year. The indexer refuses a longer
/// setting at startup; the store adds it to the clock as a `bigint`.
pub const MAX_VALID_FOR: Duration = Duration::from_secs(366 * 24 * 60 * 60);

/// The whole store: every chain any indexer has written into one database.
///
/// What the gateway serves is exactly what this holds — nothing to configure,
/// and a new indexer is served the first time it commits a window. A
/// [`ChainStore`] is one chain's view of the same pool.
#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// The store over a pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One chain's view of this store.
    pub fn chain(&self, chain_id: i64) -> ChainStore {
        ChainStore::new(self.pool.clone(), chain_id)
    }

    /// Every chain an indexer has prepared, cursor or not, with its names:
    /// what `/v1/status` and the gateway's `/ens/status` list, so a chain
    /// still in its first window is visible there.
    pub async fn chain_statuses(&self) -> Result<Vec<ChainStatusRow>, sqlx::Error> {
        sqlx::query_as(sql::CHAIN_STATUSES)
            .fetch_all(&self.pool)
            .await
    }

    /// Every chain an indexer has prepared in this store, a chain still in
    /// its first window included.
    pub async fn chains(&self) -> Result<Vec<i64>, sqlx::Error> {
        sqlx::query_scalar(sql::CHAINS).fetch_all(&self.pool).await
    }
}

/// The migrations this crate owns, embedded at compile time.
///
/// Exposed because `sqlx::migrate!` resolves its path against the crate that
/// invokes it, so anything outside this crate — a test in either binary —
/// would have to spell a relative path back to here and would break the day
/// the layout moves.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Connect and bring the schema current.
pub async fn connect_and_migrate(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPool::connect(database_url).await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// Connect without touching the schema.
///
/// For the read API, which owns nothing here. Migrating is a write, and the
/// writer is the indexer: two processes racing `sqlx::migrate!` on a fresh
/// database is a deadlock waiting to be reported as a startup flake. A reader
/// that starts before the schema exists fails its first query and gets
/// restarted, which is the right outcome and a visible one.
pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPool::connect(database_url).await?)
}

/// Why applying an event failed. Distinct from [`sqlx::Error`] because not
/// every failure is the database's: a chain value that does not fit a BIGINT
/// is this indexer's limit, and blaming the driver for it would send an
/// operator debugging the wrong layer.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The database refused or the connection failed.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    /// A refund released less than the index had booked for its sender,
    /// which the contract never does, or the booked amount is no `uint256`:
    /// the index's own books are wrong, and applying the window would hide
    /// it.
    #[error(
        "a refund released {released}, but the index booked {booked} for its sender"
    )]
    Books {
        /// What the index had booked, as stored.
        booked: String,
        /// What the refund released.
        released: U256,
    },
    /// A chain value does not fit the column that stores it.
    #[error("{what} {value} does not fit in a BIGINT")]
    OutOfRange {
        /// Which value, named the way the event names it.
        what: &'static str,
        /// The value itself.
        value: u64,
    },
}

/// Why a chain's names could not be declared.
#[derive(Debug, thiserror::Error)]
pub enum ChainNamesError {
    /// Another chain in the store already goes by this name. Nothing was
    /// written: a name has to mean one chain, and the deployment that
    /// declared it second is the one to fix.
    #[error("chain name {name:?} already belongs to chain {chain_id}")]
    Taken {
        /// The name both chains claimed.
        name: String,
        /// The chain that holds it.
        chain_id: i64,
    },
    /// The database refused or the connection failed.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The chain's row no longer names this writer: a later prepare took the
/// chain, or the row is gone.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("chain {chain_id} is no longer this writer's to write")]
pub struct Superseded {
    /// The chain.
    pub chain_id: i64,
}

/// Why a window could not open.
#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    /// This writer may no longer write the chain.
    #[error(transparent)]
    Superseded(#[from] Superseded),
    /// The database refused or the connection failed.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Why a writer lease is lost. Either way another instance may take the
/// chain, and this one is no longer its writer.
#[derive(Debug, thiserror::Error)]
pub enum LeaseLost {
    /// The lease's session failed a query: it ended, and the lock with it.
    #[error("the writer lease's session ended")]
    Ended(#[from] sqlx::Error),
    /// The lease's session did not answer in time, which its client cannot
    /// tell from an ended one.
    #[error("the writer lease's session did not answer within {0:?}")]
    Silent(Duration),
}

/// A held per-chain writer lease. One process indexes one chain at a time:
/// the Postgres advisory lock lives on this dedicated connection for the
/// process's lifetime, so a second instance — a rolling-deploy overlap, a
/// stray operator shell — blocks instead of interleaving writes with ours.
///
/// It is also a witness: [`ChainStore::prepare`] takes `&WriterLease`, so the
/// check-clear-stamp sequence cannot be run without holding the lock that
/// makes it safe. The lease remembers which chain it locks, and `prepare`
/// verifies the match, so a lease from one store cannot vouch for another.
pub struct WriterLease {
    /// The lease's own session, not a pooled one: the advisory lock belongs to
    /// the session, so a pooled connection would carry the lock back into the
    /// pool. Dropping this one closes the socket, and Postgres releases it.
    conn: sqlx::PgConnection,
    /// The chain whose advisory lock this connection holds.
    chain_id: i64,
}

impl WriterLease {
    /// Return once the lease is lost: its session ended or stopped
    /// answering. Asked every `every`, with as long again to answer; the
    /// asking also keeps the session from idling out.
    pub async fn lost(&mut self, every: Duration) -> LeaseLost {
        loop {
            tokio::time::sleep(every).await;
            let alive = sqlx::query(sql::WRITER_ALIVE).execute(&mut self.conn);
            match tokio::time::timeout(every, alive).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return LeaseLost::Ended(e),
                Err(_) => return LeaseLost::Silent(every),
            }
        }
    }

    /// Give the lock back and close the session.
    ///
    /// Dropping the lease also releases it, by closing the socket — but on
    /// Postgres's schedule rather than the caller's. Prefer this where the
    /// release has to have happened before the next statement runs, which is
    /// every test that takes a second lease on the same chain.
    pub async fn release(mut self) -> Result<(), sqlx::Error> {
        let key = format!("usernames-indexer:{}", self.chain_id);
        sqlx::query(sql::WRITER_UNLOCK)
            .bind(&key)
            .execute(&mut self.conn)
            .await?;
        self.conn.close().await
    }
}

/// All persistence for one chain: the pool and the chain id fused, so chain
/// scoping is carried by the type instead of threaded through every call.
#[derive(Clone)]
pub struct ChainStore {
    pool: PgPool,
    chain_id: i64,
}

impl ChainStore {
    /// A store scoped to one chain. Cheap to clone — clones share the pool.
    pub fn new(pool: PgPool, chain_id: i64) -> Self {
        Self { pool, chain_id }
    }

    /// The chain this store is scoped to.
    pub fn chain_id(&self) -> i64 {
        self.chain_id
    }

    /// The chain's row; all `None` before an indexer prepared it.
    pub async fn state(&self) -> Result<ChainState, sqlx::Error> {
        let state: Option<ChainState> = sqlx::query_as(sql::CHAIN_STATE)
            .bind(self.chain_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(state.unwrap_or(ChainState {
            chain_id: self.chain_id,
            ..ChainState::default()
        }))
    }

    /// Take the chain's writer lease, waiting (with a log line) if another
    /// instance still holds it.
    pub async fn acquire_writer(&self) -> Result<WriterLease, sqlx::Error> {
        // Its own session, not `pool.acquire()` — see [`WriterLease`].
        let mut conn =
            sqlx::PgConnection::connect_with(&self.pool.connect_options()).await?;
        let key = format!("usernames-indexer:{}", self.chain_id);
        let taken: bool = sqlx::query_scalar(sql::WRITER_TRY_LOCK)
            .bind(&key)
            .fetch_one(&mut conn)
            .await?;
        if !taken {
            tracing::warn!(
                chain_id = self.chain_id,
                "another indexer holds this chain's writer lock; waiting"
            );
            sqlx::query(sql::WRITER_LOCK)
                .bind(&key)
                .execute(&mut conn)
                .await?;
        }
        Ok(WriterLease {
            conn,
            chain_id: self.chain_id,
        })
    }

    /// Clear and rescan this chain when this build writes a different shape
    /// than the database holds — or when a watched contract changed, because
    /// the old contract's bindings are not the new contract's bindings, and an
    /// escrow watched from now on has events behind the cursor.
    /// Strictly scoped to this chain: a co-tenant chain's rows are
    /// untouched. The registry's deployment block survives a replay of the
    /// same registry: `eth_getCode` history does not change shape with the
    /// read model.
    ///
    /// The lease parameter is the safety argument in the signature: without
    /// the chain's writer lock, the check-clear-stamp sequence could race a
    /// still-running older instance. The writer it returns carries a new
    /// generation, which the chain's row names from then on, so whatever an
    /// earlier writer still tries to write after its lease was lost lands
    /// nowhere.
    ///
    /// A lease for a DIFFERENT chain is a hard panic, not a debug assert:
    /// proceeding would clear this chain's rows without the lock that makes
    /// the clear safe — a data-destroying race — and this runs once at
    /// startup, where crashing is cheap and the bug is a miswired process,
    /// not bad input.
    pub async fn prepare(
        &self,
        lease: &WriterLease,
        contract: Address,
        escrow: Option<Address>,
    ) -> Result<ChainWriter, sqlx::Error> {
        assert_eq!(
            lease.chain_id, self.chain_id,
            "writer lease locks chain {}, but this store prepares chain {}",
            lease.chain_id, self.chain_id
        );
        let mut tx = self.pool.begin().await?;
        sqlx::query(sql::LOCK_CHAIN)
            .bind(self.chain_id)
            .execute(&mut *tx)
            .await?;
        let known: ChainState = sqlx::query_as(sql::CHAIN_STATE)
            .bind(self.chain_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_default();
        if known.indexer_version.as_deref() == Some(INDEXER_VERSION)
            && known.contract == Some(contract)
            && known.escrow == escrow
        {
            let writer = sqlx::query_scalar(sql::CLAIM_CHAIN)
                .bind(self.chain_id)
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(ChainWriter {
                store: self.clone(),
                writer,
            });
        }
        tracing::warn!(
            chain_id = self.chain_id,
            version_from = known.indexer_version.as_deref().unwrap_or("<none>"),
            version_to = INDEXER_VERSION,
            contract_from = ?known.contract,
            contract_to = %contract,
            escrow_from = ?known.escrow,
            escrow_to = ?escrow,
            "read-model shape or a watched contract changed; clearing this chain for a full replay"
        );
        sqlx::query(sql::CLEAR_CHAIN)
            .bind(self.chain_id)
            .execute(&mut *tx)
            .await?;
        // The row starts over, so nothing it holds survives the replay unless
        // it is carried here: the deployment block, while the registry stays.
        sqlx::query(sql::FORGET_CHAIN)
            .bind(self.chain_id)
            .execute(&mut *tx)
            .await?;
        let deploy_block = known.deploy_block_of(contract);
        let writer = sqlx::query_scalar(sql::RESET_CHAIN)
            .bind(self.chain_id)
            .bind(INDEXER_VERSION)
            .bind(contract)
            .bind(escrow)
            .bind(deploy_block)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(
            chain_id = self.chain_id,
            version = INDEXER_VERSION,
            "replay armed; starts next cycle"
        );
        Ok(ChainWriter {
            store: self.clone(),
            writer,
        })
    }

    /// Declare the names this chain goes by in an ENS name, replacing whatever
    /// it declared before. Under the writer lease like [`Self::prepare`], and
    /// for the same reason: the check-clear-write must not race another
    /// instance of this chain. A name another chain holds refuses the whole
    /// set, so a misconfigured deployment writes nothing rather than half.
    pub async fn set_chain_names(
        &self,
        lease: &WriterLease,
        names: &[ChainName],
    ) -> Result<(), ChainNamesError> {
        assert_eq!(
            lease.chain_id, self.chain_id,
            "writer lease locks chain {}, but this store names chain {}",
            lease.chain_id, self.chain_id
        );
        let mut wanted: Vec<String> =
            names.iter().map(|n| n.as_str().to_string()).collect();
        wanted.sort();
        wanted.dedup();

        let mut tx = self.pool.begin().await?;
        let taken: Vec<(String, i64)> = sqlx::query_as(sql::CHAIN_NAMES_TAKEN)
            .bind(&wanted)
            .bind(self.chain_id)
            .fetch_all(&mut *tx)
            .await?;
        if let Some((name, chain_id)) = taken.into_iter().next() {
            return Err(ChainNamesError::Taken { name, chain_id });
        }
        sqlx::query(sql::CLEAR_CHAIN_NAMES)
            .bind(self.chain_id)
            .execute(&mut *tx)
            .await?;
        for name in &wanted {
            sqlx::query(sql::INSERT_CHAIN_NAME)
                .bind(name)
                .bind(self.chain_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Set when the report expires, as absolute Unix seconds: how a suite
    /// stands up an indexer that stopped reporting. Production writes
    /// validity through [`ChainWriter::set_chain_target`] and
    /// [`ChainWriter::touch_chain_target`], relative to the database's clock.
    #[cfg(feature = "test-seams")]
    pub async fn set_chain_target_valid_until(&self, secs: u64) {
        sqlx::query(sql::SET_VALID_UNTIL)
            .bind(self.chain_id)
            .bind(i64::try_from(secs).unwrap_or(i64::MAX))
            .execute(&self.pool)
            .await
            .expect("the report's expiry is written");
    }

    /// Delete every row this chain has: the read model, the chain's row and
    /// its names. How a suite starts its chain from nothing.
    #[cfg(feature = "test-seams")]
    pub async fn forget(&self) {
        let mut tx = self.pool.begin().await.expect("a transaction");
        for statement in [sql::CLEAR_CHAIN, sql::FORGET_CHAIN, sql::CLEAR_CHAIN_NAMES] {
            sqlx::query(statement)
                .bind(self.chain_id)
                .execute(&mut *tx)
                .await
                .expect("the chain is forgotten");
        }
        tx.commit().await.expect("the chain is forgotten");
    }

    /// Drop the chain's cursor and keep everything else: an index that
    /// cannot say how far it got.
    #[cfg(feature = "test-seams")]
    pub async fn forget_progress(&self) {
        sqlx::query(sql::FORGET_CURSOR)
            .bind(self.chain_id)
            .execute(&self.pool)
            .await
            .expect("the chain's cursor is dropped");
    }

    /// The last fully-processed block, if any window ever committed.
    pub async fn cursor(&self) -> Result<Option<u64>, sqlx::Error> {
        Ok(self.state().await?.position().cursor)
    }

    /// The cached deployment block of `contract` on this chain, if detected
    /// for it before.
    pub async fn deploy_block(
        &self,
        contract: Address,
    ) -> Result<Option<u64>, sqlx::Error> {
        Ok(block(self.state().await?.deploy_block_of(contract)))
    }
}

/// The right to write one chain, from [`ChainStore::prepare`]. The chain's
/// row names the writer generation that prepared it last, and a write lands
/// only while the row names this one: an instance whose lease was lost
/// cannot write over the chain of the instance that took it.
#[derive(Clone)]
pub struct ChainWriter {
    store: ChainStore,
    /// The generation `prepare` wrote into the chain's row.
    writer: i64,
}

impl ChainWriter {
    /// The chain this writer writes, for reading it.
    pub fn store(&self) -> &ChainStore {
        &self.store
    }

    /// Record how far the chain itself has grown; `/v1/status` reports the
    /// gap between this and the cursor as lag. Best effort, outside any
    /// window.
    pub async fn set_chain_head(&self, head: u64) {
        let written = sqlx::query(sql::SET_CHAIN_HEAD)
            .bind(self.store.chain_id)
            .bind(self.writer)
            .bind(i64::try_from(head).unwrap_or(i64::MAX))
            .execute(&self.store.pool)
            .await;
        if let Err(e) = written {
            tracing::warn!(%e, "failed to record the chain head");
        }
    }

    /// Record the block the cursor is actually chasing: the head minus the
    /// confirmation depth.
    ///
    /// Separate from [`Self::set_chain_head`] because the two answer different
    /// questions. The head is how far the CHAIN has grown, which is what
    /// `/v1/status` reports. This is how far the indexer INTENDS to get, and
    /// it is the only honest thing to measure a cursor against: the cursor is
    /// never advanced past it, so a fully caught-up index sits exactly here
    /// and `CONFIRMATIONS` blocks behind the head. Comparing the cursor to the
    /// head instead makes a healthy index look permanently late by the
    /// confirmation depth.
    ///
    /// Record the target, when it was reported, and how long readers may
    /// trust that report — `valid_for` from now, by the database's clock.
    /// One statement, so the three cannot disagree: a report must never
    /// vouch for a target write that failed, and no host's clock enters
    /// into it.
    ///
    /// Returns whether the write landed, so a caller renewing the report
    /// later in the same cycle can decline to vouch for a target that never
    /// made it.
    pub async fn set_chain_target(&self, target: u64, valid_for: Duration) -> bool {
        let written = sqlx::query(sql::SET_CHAIN_TARGET)
            .bind(self.store.chain_id)
            .bind(self.writer)
            .bind(i64::try_from(target).unwrap_or(i64::MAX))
            .bind(Self::valid_for_bind(valid_for))
            .execute(&self.store.pool)
            .await;
        match written {
            Ok(done) => done.rows_affected() == 1,
            Err(e) => {
                tracing::warn!(%e, "failed to record the chain target");
                false
            }
        }
    }

    /// Renew the report without moving the target. A long catch-up commits
    /// chunk by chunk under one target, and every committed chunk is proof
    /// that the loop is alive and the chain reachable; without this a healthy
    /// backfill would expire.
    pub async fn touch_chain_target(&self, valid_for: Duration) {
        let touched = sqlx::query(sql::TOUCH_CHAIN_TARGET)
            .bind(self.store.chain_id)
            .bind(self.writer)
            .bind(Self::valid_for_bind(valid_for))
            .execute(&self.store.pool)
            .await;
        if let Err(e) = touched {
            tracing::warn!(%e, "failed to renew the chain target's report");
        }
    }

    /// The validity the store will write for a report, in the whole seconds
    /// the SQL adds to its clock.
    fn valid_for_bind(valid_for: Duration) -> i64 {
        i64::try_from(valid_for.as_secs()).unwrap_or(i64::MAX)
    }

    /// Record (or clear, with `None`) the last window failure, so a stalled
    /// loop is visible in `/v1/status` instead of only in the logs. Best
    /// effort.
    pub async fn set_window_error(&self, error: Option<&str>) {
        let written = sqlx::query(sql::SET_WINDOW_ERROR)
            .bind(self.store.chain_id)
            .bind(self.writer)
            .bind(error)
            .execute(&self.store.pool)
            .await;
        if let Err(e) = written {
            tracing::warn!(%e, "failed to record the window error state");
        }
    }

    /// Cache a detected deployment block so a re-index does not redo the
    /// binary search. The chain's row names the contract it belongs to.
    pub async fn set_deploy_block(&self, block: u64) -> Result<(), sqlx::Error> {
        sqlx::query(sql::SET_DEPLOY_BLOCK)
            .bind(self.store.chain_id)
            .bind(self.writer)
            .bind(i64::try_from(block).unwrap_or(i64::MAX))
            .execute(&self.store.pool)
            .await?;
        Ok(())
    }

    /// Open a window. Everything applied through it commits atomically
    /// with the cursor when [`Window::commit`] runs, or not at all.
    pub async fn begin_window(&self) -> Result<Window, WindowError> {
        let chain_id = self.store.chain_id;
        let mut tx = self.store.pool.begin().await?;
        let held: Option<bool> = sqlx::query_scalar(sql::HOLD_CHAIN)
            .bind(chain_id)
            .bind(self.writer)
            .fetch_optional(&mut *tx)
            .await?;
        if held.is_none() {
            return Err(Superseded { chain_id }.into());
        }
        Ok(Window {
            tx,
            chain_id,
            bind: None,
        })
    }
}

#[cfg(test)]
impl ChainWriter {
    /// A writer no chain's row names, so nothing it writes lands.
    pub(crate) fn unprepared(store: ChainStore) -> Self {
        Self { store, writer: 0 }
    }
}

fn as_i64(value: u64, what: &'static str) -> Result<i64, ApplyError> {
    i64::try_from(value).map_err(|_| ApplyError::OutOfRange { what, value })
}

/// Where a log sat, in the columns the store keeps it in.
pub(crate) struct Position {
    pub(crate) block: i64,
    pub(crate) log_index: i64,
    pub(crate) tx_hash: B256,
    pub(crate) block_time: i64,
}

impl Position {
    fn of(pos: &LogPosition) -> Result<Self, ApplyError> {
        Ok(Self {
            block: as_i64(pos.block_number, "block number")?,
            log_index: as_i64(pos.log_index, "log index")?,
            tx_hash: pos.tx_hash,
            block_time: as_i64(pos.block_time, "block timestamp")?,
        })
    }
}

/// One open window: a transaction that only knows how to apply events
/// and how to commit together with the cursor. That the cursor cannot be
/// left behind — or advanced without its writes — is not a convention here,
/// it is the shape of the API.
pub struct Window {
    tx: Transaction<'static, Postgres>,
    chain_id: i64,
    /// The last bind this window took up, applied or replayed. A bind's
    /// `CeremonyBound` and `BindFeePaid` follow its `IdentityBound` in the
    /// same transaction, and a window holds whole blocks.
    bind: Option<history::Bind>,
}

impl Window {
    /// Advance the cursor and commit everything applied so far as one
    /// transaction: the cursor and the writes it stands for land together or
    /// not at all.
    pub async fn commit(mut self, through_block: u64) -> Result<(), sqlx::Error> {
        sqlx::query(sql::SET_CURSOR)
            .bind(self.chain_id)
            .bind(i64::try_from(through_block).unwrap_or(i64::MAX))
            .execute(&mut *self.tx)
            .await?;
        self.tx.commit().await
    }

    /// Apply one decoded event: journal row, projection writes in the order
    /// the contract wrote its own storage, and the rows that put the event in
    /// the histories of the addresses and the handle it involves. `Ok(false)`
    /// means the journal already held this (chain, block, log) — an earlier
    /// window applied it and committed — so the projections were left alone
    /// and the caller should not count it as new.
    pub async fn apply(
        &mut self,
        event: &NamesEvent,
        pos: &LogPosition,
    ) -> Result<bool, ApplyError> {
        let chain_id = self.chain_id;
        let at = Position::of(pos)?;

        // Read before the projections move: a bind's previous holders are the
        // rows it is about to overwrite, and the journal row files the event
        // under the handle it concerns.
        let involvement = self.involvement(event, &at).await?;

        let row = EventColumns::of(event, involvement.handle_node)?;
        if !row.journal(&mut self.tx, chain_id, &at).await? {
            // Running the projection writes again would double-count every
            // escrowed amount, so the journal's conflict is the one
            // idempotency gate for everything.
            return Ok(false);
        }

        match event {
            NamesEvent::IdentityBound {
                holder,
                id_node,
                handle_node,
                platform_id,
                observed_at,
                ceremony_version,
            } => {
                let observed = as_i64(*observed_at, "observedAt")?;
                sqlx::query(sql::UPSERT_ID)
                    .bind(chain_id)
                    .bind(id_node)
                    .bind(platform_id)
                    .bind(holder)
                    .bind(observed)
                    .bind(i64::from(*ceremony_version))
                    .bind(handle_node)
                    .execute(&mut *self.tx)
                    .await?;
                sqlx::query(sql::UPSERT_HANDLE)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(platform_id)
                    .bind(holder)
                    .bind(observed)
                    .bind(i64::from(*ceremony_version))
                    .bind(id_node)
                    .execute(&mut *self.tx)
                    .await?;
            }

            // Its text goes beside its node with the histories; it moves
            // nothing else.
            NamesEvent::HandleRevealed { .. } => {}

            // The holder's one name, on whichever platform: a name on
            // another platform is replaced, and the contract logs no
            // HandleUnpublished for it.
            NamesEvent::HandlePublished {
                holder,
                platform_id,
                handle_node,
                ..
            } => {
                sqlx::query(sql::PUBLISH)
                    .bind(chain_id)
                    .bind(holder)
                    .bind(platform_id)
                    .bind(handle_node)
                    .execute(&mut *self.tx)
                    .await?;
            }

            NamesEvent::HandleRetired {
                platform_id: _,
                handle_node,
                holder: _,
            } => {
                sqlx::query(sql::RETIRE_HANDLE)
                    .bind(chain_id)
                    .bind(handle_node)
                    .execute(&mut *self.tx)
                    .await?;
            }

            NamesEvent::HandleUnpublished { holder } => {
                sqlx::query(sql::WITHDRAW_NAME)
                    .bind(chain_id)
                    .bind(holder)
                    .execute(&mut *self.tx)
                    .await?;
            }

            NamesEvent::ProofVerifierConfigured { verifier } => {
                sqlx::query(sql::SET_PROOF_VERIFIER)
                    .bind(chain_id)
                    .bind(verifier)
                    .execute(&mut *self.tx)
                    .await?;
            }

            // No projection of their own: nothing resolves by a ceremony's
            // client or a bind's fee, and the journal and histories hold both.
            NamesEvent::CeremonyBound { .. } | NamesEvent::BindFeePaid { .. } => {}

            NamesEvent::Escrow(escrow) => {
                self.book_escrow(escrow, &at).await?;
            }
        }

        self.record(involvement, &at).await?;
        Ok(true)
    }
}

// ─── The read side ──────────────────────────────────────────────────────────
// The same store the writer uses answers the API's queries, so every
// statement — and the join shapes the projections were designed for — is
// one file under `sql/`, bound here. Every read spans the chains the store
// holds, or one of them when
// the caller names it: `chain` is `None` for every chain, and every row says
// which chain it came from. Handlers translate HTTP to these calls and rows
// to JSON; they do not own queries.

/// One `names.handles` row and its disclosed text: everything `resolveHandle`
/// answers from.
#[derive(sqlx::FromRow)]
pub struct HandleRow {
    /// The chain the handle is bound on.
    pub chain_id: i64,
    /// The platform the handle lives on.
    pub platform_id: B256,
    /// The holder, or `None` after retirement — the contract's `address(0)`.
    pub owner: Option<Address>,
    /// The proof-freshness watermark, kept even through retirement.
    pub observed_at: i64,
    /// The ceremony version that proved the last binding at this node. Kept
    /// through retirement: the contract never stored it, so the index does.
    pub ceremony_version: i64,
    /// The account id node this handle points back at (`idNodeByHandle`).
    pub id_node: B256,
    /// The handle's text, once the chain disclosed it.
    pub handle: Option<String>,
}

/// One `names.ids` row joined with the handle node it points at and the
/// holder's name: everything `resolveId` and the reverse display answer
/// from.
#[derive(sqlx::FromRow)]
pub struct IdentityRow {
    /// The chain the account is bound on.
    pub chain_id: i64,
    /// The platform the account lives on.
    pub platform_id: B256,
    /// The storage key the chain filed this account under.
    pub id_node: B256,
    /// The holder that proved the account.
    pub owner: Address,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// The handle node this account last proved (`handleNodeById`).
    pub handle_node: B256,
    /// That handle's text, once the chain disclosed it.
    pub handle: Option<String>,
    /// The holder the handle node currently resolves to.
    pub handle_owner: Option<Address>,
    /// The id node the handle currently points back at.
    pub handle_id_node: Option<B256>,
    /// Whether the owner's name is this handle, on this platform.
    pub published: bool,
}

impl IdentityRow {
    /// The `handleNodeById`/`idNodeByHandle` round trip, written once: the
    /// handle is presentable only while the node still points back at this
    /// id and still resolves to this wallet. Otherwise the account renamed
    /// or was overtaken, and showing the stale handle would mis-route a
    /// payment.
    pub fn handle_still_owned(&self) -> bool {
        self.handle_owner == Some(self.owner) && self.handle_id_node == Some(self.id_node)
    }

    /// The handle's text to present: disclosed, and still this account's.
    pub fn presentable_handle(&self) -> Option<&str> {
        if self.handle_still_owned() {
            self.handle.as_deref()
        } else {
            None
        }
    }

    /// Whether this is the wallet's name — only while the handle is still
    /// the account's, as `primaryHandleOf` answers.
    pub fn displayed(&self) -> bool {
        self.published && self.handle_still_owned()
    }
}

/// One search hit: a live, disclosed handle, its owner, and the id node it
/// pairs with.
#[derive(sqlx::FromRow)]
pub struct SearchRow {
    /// The chain the handle is bound on.
    pub chain_id: i64,
    /// The platform the handle lives on.
    pub platform_id: B256,
    /// The storage key the chain filed the handle under.
    pub handle_node: B256,
    /// The account id node behind it (`idNodeByHandle`).
    pub id_node: B256,
    /// The handle's text.
    pub handle: String,
    /// The holder it resolves to (retired handles never match).
    pub owner: Address,
    /// Whether this handle is the owner's name.
    pub published: bool,
}

/// Escape LIKE metacharacters so query text matches itself.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl Store {
    /// The chain a name belongs to, if any chain's indexer declared it.
    pub async fn chain_named(&self, name: &str) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar(sql::CHAIN_NAMED)
            .bind(name)
            .fetch_optional(&self.pool)
            .await
    }

    /// Whether an indexer has committed a first window — on the chain named,
    /// or on any chain when none is. Before that there is nothing to answer
    /// from, and a 404 would claim more than the store knows.
    pub async fn synced(&self, chain: Option<i64>) -> Result<bool, sqlx::Error> {
        let mut statement = QueryBuilder::new(sql::SYNCED);
        scoped(&mut statement, "chain_id", chain);
        statement.push(" LIMIT 1");
        let found: Option<bool> = statement
            .build_query_scalar()
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }

    /// The rows `resolveHandle` answers from, one per chain the handle node
    /// is bound on.
    pub async fn resolve_handle(
        &self,
        chain: Option<i64>,
        handle_node: B256,
    ) -> Result<Vec<HandleRow>, sqlx::Error> {
        handle_lookup("", chain, handle_node)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// The rows `resolveId` answers from, one per chain the id node is bound
    /// on.
    pub async fn resolve_id(
        &self,
        chain: Option<i64>,
        id_node: B256,
    ) -> Result<Vec<IdentityRow>, sqlx::Error> {
        id_lookup("", chain, id_node)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// Every identity a wallet proved, chain by chain and platform by
    /// platform, each saying whether its handle is the wallet's name
    /// (`primaryHandleOf`).
    pub async fn identities_of(
        &self,
        chain: Option<i64>,
        owner: Address,
    ) -> Result<Vec<IdentityRow>, sqlx::Error> {
        identities_lookup("", chain, owner)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// Live, disclosed handles a search lists, across the chains in scope:
    /// those a wallet holds, those matching a folded partial query — exact
    /// first, then prefix, then substring, then trigram-fuzzy — or both.
    pub async fn search_handles(
        &self,
        chain: Option<i64>,
        platform_id: Option<B256>,
        owner: Option<Address>,
        folded_query: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<SearchRow>, sqlx::Error> {
        search_lookup("", chain, platform_id, owner, folded_query, limit, offset)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }
}

impl ChainStore {
    /// What the ENS gateway answers for a handle node on this chain: where
    /// the index stands and who holds the node, from one snapshot, so a
    /// replay clearing the chain between the two cannot be signed for.
    /// `ens_answer.sql` says which bindings it answers for.
    pub async fn ens_answer(&self, handle_node: B256) -> Result<EnsAnswer, sqlx::Error> {
        let row: Option<EnsRow> = sqlx::query_as(sql::ENS_ANSWER)
            .bind(self.chain_id)
            .bind(handle_node)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(row) => EnsAnswer {
                position: Some(row.state.position()),
                holder: row.owner,
            },
            None => EnsAnswer {
                position: None,
                holder: None,
            },
        })
    }
}

// ─── The read statements ────────────────────────────────────────────────────
// Each read over the big tables composes its WHERE clause per request, so
// every shape is its own prepared statement with its own plan. A fixed
// `($1 IS NULL OR col = $1)` would not do: the generic plan a prepared
// statement settles on cannot drop the branch, and the index the lookup was
// written for goes unused. The builders take a prefix so the plan test can
// put `EXPLAIN` in front of the exact statement the store runs.

type Statement = QueryBuilder<'static, Postgres>;

/// `AND column = chain`, when a chain is named.
fn scoped(statement: &mut Statement, column: &'static str, chain: Option<i64>) {
    if let Some(chain) = chain {
        statement
            .push(" AND ")
            .push(column)
            .push(" = ")
            .push_bind(chain);
    }
}

fn handle_lookup(prefix: &str, chain: Option<i64>, handle_node: B256) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} WHERE h.handle_node = ",
        sql::HANDLE_PROJECTION
    ));
    statement.push_bind(handle_node);
    scoped(&mut statement, "h.chain_id", chain);
    statement.push(" ORDER BY h.chain_id");
    statement
}

fn id_lookup(prefix: &str, chain: Option<i64>, id_node: B256) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} WHERE i.id_node = ",
        sql::IDENTITY_PROJECTION
    ));
    statement.push_bind(id_node);
    scoped(&mut statement, "i.chain_id", chain);
    statement.push(" ORDER BY i.chain_id");
    statement
}

fn identities_lookup(prefix: &str, chain: Option<i64>, owner: Address) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} WHERE i.owner = ",
        sql::IDENTITY_PROJECTION
    ));
    statement.push_bind(owner);
    scoped(&mut statement, "i.chain_id", chain);
    statement.push(" ORDER BY i.chain_id, i.platform_id, i.id_node");
    statement
}

/// The search: live handles, narrowed by whatever the caller named, ranked
/// by the text when there is one — exact, prefix, substring, then trigram
/// similarity — and by handle and chain otherwise. LIKE-escaping happens
/// here so the query text matches itself, which is SQL knowledge.
fn search_lookup(
    prefix: &str,
    chain: Option<i64>,
    platform_id: Option<B256>,
    owner: Option<Address>,
    folded_query: Option<&str>,
    limit: i64,
    offset: i64,
) -> Statement {
    let mut statement = QueryBuilder::new(format!("{prefix}{}", sql::SEARCH_PROJECTION));
    scoped(&mut statement, "h.chain_id", chain);
    if let Some(platform_id) = platform_id {
        statement
            .push(" AND v.platform_id = ")
            .push_bind(platform_id);
    }
    if let Some(owner) = owner {
        statement.push(" AND h.owner = ").push_bind(owner);
    }
    match folded_query {
        Some(query) => {
            let like = escape_like(query);
            statement
                .push(" AND (v.handle LIKE '%' || ")
                .push_bind(like.clone())
                .push(r#" || '%' ESCAPE '\' OR v.handle % "#)
                .push_bind(query.to_string())
                .push(") ORDER BY (v.handle = ")
                .push_bind(query.to_string())
                .push(") DESC, (v.handle LIKE ")
                .push_bind(like.clone())
                .push(r#" || '%' ESCAPE '\') DESC, (v.handle LIKE '%' || "#)
                .push_bind(like)
                .push(r#" || '%' ESCAPE '\') DESC, similarity(v.handle, "#)
                .push_bind(query.to_string())
                .push(") DESC, v.handle ASC, h.chain_id ASC");
        }
        None => {
            statement.push(" ORDER BY v.handle ASC, h.chain_id ASC");
        }
    }
    statement
        .push(" LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    statement
}

/// Every statement the store runs, one file each under `sql/`, named by
/// what it does. The Rust beside a use binds the parameters and says why;
/// the file says what. Read at compile time, so a missing file fails the
/// build rather than a request.
mod sql {
    pub const CHAIN_STATE: &str = include_str!("../sql/chain_state.sql");
    pub const CHAIN_STATUSES: &str = include_str!("../sql/chain_statuses.sql");
    pub const CHAINS: &str = include_str!("../sql/chains.sql");
    pub const LOCK_CHAIN: &str = include_str!("../sql/lock_chain.sql");
    pub const CLAIM_CHAIN: &str = include_str!("../sql/claim_chain.sql");
    pub const RESET_CHAIN: &str = include_str!("../sql/reset_chain.sql");
    pub const HOLD_CHAIN: &str = include_str!("../sql/hold_chain.sql");
    pub const SET_CURSOR: &str = include_str!("../sql/set_cursor.sql");
    pub const SET_CHAIN_HEAD: &str = include_str!("../sql/set_chain_head.sql");
    pub const SET_DEPLOY_BLOCK: &str = include_str!("../sql/set_deploy_block.sql");
    pub const SET_PROOF_VERIFIER: &str = include_str!("../sql/set_proof_verifier.sql");
    pub const SET_WINDOW_ERROR: &str = include_str!("../sql/set_window_error.sql");
    #[cfg(feature = "test-seams")]
    pub const SET_VALID_UNTIL: &str = include_str!("../sql/set_valid_until.sql");
    #[cfg(feature = "test-seams")]
    pub const FORGET_CURSOR: &str = include_str!("../sql/forget_cursor.sql");
    pub const FORGET_CHAIN: &str = include_str!("../sql/forget_chain.sql");
    /// Every row an indexer wrote for one chain into the read model: what a
    /// replay clears.
    pub const CLEAR_CHAIN: &str = include_str!("../sql/clear_chain.sql");
    pub const WRITER_TRY_LOCK: &str = include_str!("../sql/writer_try_lock.sql");
    pub const WRITER_LOCK: &str = include_str!("../sql/writer_lock.sql");
    pub const WRITER_UNLOCK: &str = include_str!("../sql/writer_unlock.sql");
    pub const WRITER_ALIVE: &str = include_str!("../sql/writer_alive.sql");
    pub const SET_CHAIN_TARGET: &str = include_str!("../sql/set_chain_target.sql");
    pub const TOUCH_CHAIN_TARGET: &str = include_str!("../sql/touch_chain_target.sql");
    pub const JOURNAL_EVENT: &str = include_str!("../sql/journal_event.sql");
    pub const UPSERT_ID: &str = include_str!("../sql/upsert_id.sql");
    pub const UPSERT_HANDLE: &str = include_str!("../sql/upsert_handle.sql");
    pub const PUBLISH: &str = include_str!("../sql/publish.sql");
    pub const WITHDRAW_NAME: &str = include_str!("../sql/withdraw_name.sql");
    pub const PUBLISHED_NAME: &str = include_str!("../sql/published_name.sql");
    pub const RETIRE_HANDLE: &str = include_str!("../sql/retire_handle.sql");
    pub const ENS_ANSWER: &str = include_str!("../sql/ens_answer.sql");
    pub const CHAIN_NAMES_TAKEN: &str = include_str!("../sql/chain_names_taken.sql");
    /// The names the chain goes by in an ENS name.
    pub const CLEAR_CHAIN_NAMES: &str = include_str!("../sql/clear_chain_names.sql");
    pub const INSERT_CHAIN_NAME: &str = include_str!("../sql/insert_chain_name.sql");
    pub const CHAIN_NAMED: &str = include_str!("../sql/chain_named.sql");
    pub const SYNCED: &str = include_str!("../sql/synced.sql");
    /// The projection every `names.ids` read shares: the row itself, the
    /// handle it points at, and whether that pair is displayed. A caller
    /// appends its own `WHERE`, the only part that differs.
    pub const IDENTITY_PROJECTION: &str = include_str!("../sql/identity_projection.sql");
    /// The projection every `names.handles` read shares: the row and its
    /// disclosed text. A caller appends its `WHERE`.
    pub const HANDLE_PROJECTION: &str = include_str!("../sql/handle_projection.sql");
    /// Live, disclosed handles with their id node and display flag; the caller
    /// narrows, orders and pages.
    pub const SEARCH_PROJECTION: &str = include_str!("../sql/search_projection.sql");
    pub const ESCROW_DEPOSIT: &str = include_str!("../sql/escrow_deposit.sql");
    pub const ESCROW_BOOK_REFUNDABLE: &str =
        include_str!("../sql/escrow_book_refundable.sql");
    pub const ESCROW_CLAIM: &str = include_str!("../sql/escrow_claim.sql");
    pub const ESCROW_CLOSE_ROUND: &str = include_str!("../sql/escrow_close_round.sql");
    pub const ESCROW_REFUND: &str = include_str!("../sql/escrow_refund.sql");
    pub const ESCROW_TAKE_REFUNDABLE: &str =
        include_str!("../sql/escrow_take_refundable.sql");
    pub const PREVIOUS_HOLDERS: &str = include_str!("../sql/previous_holders.sql");
    pub const CLAIMED_SENDERS: &str = include_str!("../sql/claimed_senders.sql");
    pub const RECORD_ADDRESS_EVENT: &str =
        include_str!("../sql/record_address_event.sql");
    pub const RECORD_HANDLE_NODE: &str = include_str!("../sql/record_handle_node.sql");
    /// An address's history: the events it took part in, each with its
    /// roles, the journal row and the handle the event concerns. The caller
    /// names the address, narrows and pages.
    pub const ADDRESS_HISTORY_PROJECTION: &str =
        include_str!("../sql/address_history_projection.sql");
    /// A handle's history, in the same shape without roles.
    pub const HANDLE_HISTORY_PROJECTION: &str =
        include_str!("../sql/handle_history_projection.sql");
    /// Slots still holding something, with the handle and its holder; the
    /// caller narrows, orders and pages.
    pub const ESCROW_SLOT_PROJECTION: &str =
        include_str!("../sql/escrow_slot_projection.sql");
    /// The same shape, driven from the handles an address holds.
    pub const CLAIMABLE_PROJECTION: &str =
        include_str!("../sql/claimable_projection.sql");
    /// One sender's contributions in the same shape, with their refund dates.
    pub const REFUNDABLE_PROJECTION: &str =
        include_str!("../sql/refundable_projection.sql");
}

#[cfg(test)]
pub(crate) mod plan_tests {
    use sqlx::PgConnection;

    use super::*;

    /// Rows in the shape production holds them, on the chain every probe
    /// names; the file says how they are shaped.
    const VOLUME: &str = include_str!("../sql/plan_volume.sql");

    /// One statement's plan, and the indexes it must seek: `a|b` where either
    /// seeks exactly, `None` where only the row count decides between them.
    pub(crate) struct Plan {
        pub(crate) name: &'static str,
        pub(crate) seeks: Option<&'static [&'static str]>,
        pub(crate) text: String,
    }

    impl Plan {
        /// The plan of a composed lookup, built with an `EXPLAIN ` prefix.
        pub(crate) async fn of(
            conn: &mut PgConnection,
            name: &'static str,
            seeks: Option<&'static [&'static str]>,
            mut statement: Statement,
        ) -> Self {
            let text = statement
                .build_query_scalar::<String>()
                .fetch_all(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .join("\n");
            Self { name, seeks, text }
        }
    }

    /// `EXPLAIN` in front of a statement file.
    pub(crate) fn explain(sql: &str) -> String {
        format!("EXPLAIN {sql}")
    }

    /// The resolution and search lookups, in every shape the API asks them.
    async fn plans(conn: &mut PgConnection) -> Vec<Plan> {
        let platform = B256::repeat_byte(1);
        let node = B256::repeat_byte(3);
        let owner = Address::repeat_byte(2);
        let mut plans = Vec::new();
        for (name, seeks, statement) in [
            (
                "handle node, any chain",
                Some(&["handles_node_chain_idx"][..]),
                handle_lookup("EXPLAIN ", None, node),
            ),
            (
                "handle node, one chain",
                Some(&["handles_pkey|handles_node_chain_idx"][..]),
                handle_lookup("EXPLAIN ", Some(1), node),
            ),
            (
                "id node, any chain",
                Some(&["ids_node_chain_idx"][..]),
                id_lookup("EXPLAIN ", None, node),
            ),
            (
                "id node, one chain",
                Some(&["ids_pkey|ids_node_chain_idx"][..]),
                id_lookup("EXPLAIN ", Some(1), node),
            ),
            (
                "wallet, any chain",
                Some(&["ids_owner_chain_idx"][..]),
                identities_lookup("EXPLAIN ", None, owner),
            ),
            (
                "wallet, one chain",
                Some(&["ids_owner_chain_idx"][..]),
                identities_lookup("EXPLAIN ", Some(1), owner),
            ),
            (
                "search by wallet, any chain",
                Some(&["handles_owner_chain_node_idx"][..]),
                search_lookup("EXPLAIN ", None, None, Some(owner), None, 10, 0),
            ),
            (
                "search by wallet on a platform, one chain",
                Some(&["handles_owner_chain_node_idx"][..]),
                search_lookup(
                    "EXPLAIN ",
                    Some(1),
                    Some(platform),
                    Some(owner),
                    None,
                    10,
                    0,
                ),
            ),
            (
                "search by text, any chain",
                Some(&["handle_nodes_handle_trgm_idx"][..]),
                search_lookup("EXPLAIN ", None, None, None, Some("ali"), 10, 0),
            ),
            (
                "search by text on a platform, one chain",
                Some(&["handle_nodes_handle_trgm_idx"][..]),
                search_lookup(
                    "EXPLAIN ",
                    Some(1),
                    Some(platform),
                    None,
                    Some("ali"),
                    10,
                    0,
                ),
            ),
        ] {
            plans.push(Plan::of(conn, name, seeks, statement).await);
        }

        let reads = [
            (
                "the gateway's answer",
                &["handles_pkey|handles_node_chain_idx"][..],
                sqlx::query_scalar::<_, String>(&explain(sql::ENS_ANSWER))
                    .bind(1i64)
                    .bind(node)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "a withdrawn name",
                &["published_pkey"][..],
                sqlx::query_scalar(&explain(sql::WITHDRAW_NAME))
                    .bind(1i64)
                    .bind(owner)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "the name an unpublish withdraws",
                &["published_pkey"][..],
                sqlx::query_scalar(&explain(sql::PUBLISHED_NAME))
                    .bind(1i64)
                    .bind(owner)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "a name published over another",
                &["published_pkey"][..],
                sqlx::query_scalar(&explain(sql::PUBLISH))
                    .bind(1i64)
                    .bind(owner)
                    .bind(platform)
                    .bind(node)
                    .fetch_all(&mut *conn)
                    .await,
            ),
        ];
        for (name, seeks, text) in reads {
            let text: Vec<String> = text.unwrap_or_else(|e| panic!("{name}: {e}"));
            plans.push(Plan {
                name,
                seeks: Some(seeks),
                text: text.join("\n"),
            });
        }
        plans
    }

    /// Every read over the big tables seeks the index built for it, in every
    /// shape it is asked in, the indexer's own reads included. The index is
    /// named, not just "some index": a lookup missing its leading column
    /// walks a whole index and still reads as an index scan.
    ///
    /// The plans are taken over [`VOLUME`], inside a transaction that rolls
    /// back with its statistics: on a handful of rows every index costs the
    /// same. Sequential scans stay switched off so none hides behind a small
    /// table.
    #[tokio::test]
    async fn every_read_over_the_big_tables_seeks_its_index() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "DATABASE_URL not set under CI"
            );
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let pool = PgPool::connect(&url).await.expect("connect");
        MIGRATOR.run(&pool).await.expect("migrations");
        let mut conn = pool.begin().await.expect("transaction");
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *conn)
            .await
            .expect("session setting");
        sqlx::raw_sql(VOLUME)
            .execute(&mut *conn)
            .await
            .expect("the volume loads");

        let mut all = plans(&mut conn).await;
        all.extend(history::plans::all(&mut conn).await);
        all.extend(escrow::plans::all(&mut conn).await);
        for plan in all {
            let Plan { name, seeks, text } = plan;
            for index in seeks.unwrap_or_default() {
                assert!(
                    index.split('|').any(|index| text.contains(index)),
                    "{name} does not seek {index}:\n{text}"
                );
            }
            for table in [
                "handles",
                "ids",
                "published",
                "events",
                "escrow_held",
                "escrow_refundable",
                "address_events",
                "handle_nodes",
            ] {
                assert!(
                    !text.contains(&format!("Seq Scan on {table}")),
                    "{name} scans {table}:\n{text}"
                );
            }
        }
        conn.rollback().await.expect("rollback")
    }
}
