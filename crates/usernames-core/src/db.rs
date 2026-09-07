//! Postgres, behind types that carry the invariants.
//!
//! [`ChainStore`] fuses the pool with the chain it scopes every query to, so
//! no call site can pass the wrong chain id or forget it. [`Window`] is an
//! open poll window: applying events and committing-with-cursor are its only
//! operations, which makes "the cursor and the writes it stands for commit
//! together or not at all" a fact of the type rather than caller discipline.
//! Every write in [`Window::apply`] is an idempotent upsert keyed by what the
//! contract keys its own storage by, so replaying a window — after a crash,
//! or after a version-bump re-index — converges instead of duplicating.

use alloy::primitives::{
    Address,
    B256,
};
use sqlx::{
    Connection,
    PgPool,
    Postgres,
    Transaction,
};
use tracing::{
    error,
    info,
    warn,
};

use crate::{
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes::{
        self,
        NormalizedHandle,
    },
};

/// Bump on any change to what the indexer writes. A mismatch at startup
/// clears the chain's rows and cursor, so the next loop replays the chain
/// from the deployment block — the re-index IS the migration.
pub const INDEXER_VERSION: &str = "1";

/// Every projection table, in one place. [`ChainStore::prepare`] clears them
/// for a replay and the tests clean them between scenarios; a single list
/// means a new table cannot be wiped in one place and silently survive in
/// another.
pub const PROJECTION_TABLES: &[&str] = &[
    "events",
    "ids",
    "handles",
    "published",
    "platforms",
    "verifiers",
];

// The chain_metadata keys. Chain scoping is the table's chain_id column;
// only the deployment-block cache still carries anything in its key.
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CONTRACT_KEY: &str = "contract";
const CURSOR_KEY: &str = "cursor";
const HEAD_KEY: &str = "head";
const TARGET_KEY: &str = "target";
const TARGET_REPORTED_AT_KEY: &str = "target_reported_at";
const TARGET_VALID_UNTIL_KEY: &str = "target_valid_until";
const WINDOW_ERROR_KEY: &str = "window_error";

/// The mirror's standing, as [`ChainStore::mirror_position`] reads it, every
/// moment by the database's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct MirrorPosition {
    /// The block the indexer last set out to reach.
    pub target: Option<u64>,
    /// The block the cursor has committed through.
    pub cursor: Option<u64>,
    /// Unix seconds at which the indexer last reported.
    pub reported_at: Option<u64>,
    /// Seconds until the indexer's last report expires; negative once it has.
    pub valid_for: Option<i64>,
}

/// The longest validity the store will write: ten years. Anything larger is
/// a mistake, and a value near `i64::MAX` would overflow the `bigint` addition
/// in Postgres and fail the whole statement, target included.
const MAX_VALID_FOR_SECS: u64 = 10 * 366 * 24 * 60 * 60;

fn valid_for_bind(valid_for_secs: u64) -> i64 {
    i64::try_from(valid_for_secs.min(MAX_VALID_FOR_SECS)).unwrap_or(i64::MAX)
}

/// Every chain an indexer has written into this store, by cursor. What the
/// gateway serves is exactly this: nothing to configure, and a new indexer is
/// served the first time it commits a window.
pub async fn indexed_chains(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT chain_id FROM names.chain_metadata WHERE key = $1 ORDER BY chain_id",
    )
    .bind(CURSOR_KEY)
    .fetch_all(pool)
    .await
}

fn deploy_block_key(contract: Address) -> String {
    // The address is part of the key: repointing the indexer at a different
    // contract must not inherit the old contract's deployment block.
    format!("deploy_block:{contract}")
}

/// The migrations this crate owns, embedded at compile time.
///
/// Exposed because `sqlx::migrate!` resolves its path against the crate that
/// invokes it, so anything outside this crate — a test in either binary —
/// would have to spell a relative path back to here and would break the day
/// the layout moves.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

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
#[derive(Debug)]
pub enum ApplyError {
    /// The database refused or the connection failed.
    Db(sqlx::Error),
    /// A chain value does not fit the column that stores it.
    OutOfRange {
        /// Which value, named the way the event names it.
        what: &'static str,
        /// The value itself.
        value: u64,
    },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {e}"),
            Self::OutOfRange { what, value } => {
                write!(f, "{what} {value} does not fit in a BIGINT")
            }
        }
    }
}

impl std::error::Error for ApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
            Self::OutOfRange { .. } => None,
        }
    }
}

impl From<sqlx::Error> for ApplyError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
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
    /// A connection of its OWN, deliberately not one from the pool. An advisory
    /// lock taken with `pg_try_advisory_lock` is held for the SESSION, and a
    /// pooled connection outlives the lease: dropping it returned a live
    /// session, still holding the lock, to the idle pool, where it sat for the
    /// idle timeout. A second indexer — or the next test — then blocked on a
    /// lock nobody was using. Here the session is the lease's own, so dropping
    /// it closes the socket and Postgres releases the lock.
    conn: sqlx::PgConnection,
    /// The chain whose advisory lock this connection holds.
    chain_id: i64,
}

impl WriterLease {
    /// Give the lock back and close the session.
    ///
    /// Dropping the lease also releases it, by closing the socket — but on
    /// Postgres's schedule rather than the caller's. Prefer this where the
    /// release has to have happened before the next statement runs, which is
    /// every test that takes a second lease on the same chain.
    pub async fn release(mut self) -> Result<(), sqlx::Error> {
        let key = format!("usernames-indexer:{}", self.chain_id);
        sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
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

    /// The pool behind this store, for a caller that needs a query this type
    /// does not offer — a test arranging a state the writer would produce.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn get_metadata(&self, key: &str) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT value FROM names.chain_metadata WHERE chain_id = $1 AND key = $2",
        )
        .bind(self.chain_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
    }

    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO names.chain_metadata (chain_id, key, value)
               VALUES ($1, $2, $3)
               ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value"#,
        )
        .bind(self.chain_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Take the chain's writer lease, waiting (with a log line) if another
    /// instance still holds it.
    pub async fn acquire_writer(&self) -> Result<WriterLease, sqlx::Error> {
        // Its own session, not `pool.acquire()` — see [`WriterLease`].
        let mut conn =
            sqlx::PgConnection::connect_with(&self.pool.connect_options()).await?;
        let key = format!("usernames-indexer:{}", self.chain_id);
        let taken: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
                .bind(&key)
                .fetch_one(&mut conn)
                .await?;
        if !taken {
            warn!(
                chain_id = self.chain_id,
                "another indexer holds this chain's writer lock; waiting"
            );
            sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
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
    /// than the database holds — or when the watched contract changed, because
    /// the old contract's bindings are not the new contract's bindings.
    /// Strictly scoped to this chain: a co-tenant chain's rows, cursor and
    /// metadata are untouched. The deployment-block cache survives a version
    /// bump (it is keyed by contract, and `eth_getCode` history does not
    /// change shape with the read model).
    ///
    /// The lease parameter is the safety argument in the signature: without
    /// the chain's writer lock, the check-clear-stamp sequence could race a
    /// still-running older instance.
    ///
    /// A lease for a DIFFERENT chain is a hard panic, not a debug assert:
    /// proceeding would clear this chain's rows without the lock that makes
    /// the clear safe — a data-destroying race — and this runs once at
    /// startup, where crashing is cheap and the bug is a miswired process,
    /// not bad input.
    pub async fn prepare(
        &self,
        writer: &WriterLease,
        contract: Address,
    ) -> Result<(), sqlx::Error> {
        assert_eq!(
            writer.chain_id, self.chain_id,
            "writer lease locks chain {}, but this store prepares chain {}",
            writer.chain_id, self.chain_id
        );
        let version = self.get_metadata(SCHEMA_VERSION_KEY).await?;
        let known_contract = self.get_metadata(CONTRACT_KEY).await?;
        let contract_now = contract.to_string().to_lowercase();
        let version_ok = version.as_deref() == Some(INDEXER_VERSION);
        let contract_ok = known_contract.as_deref() == Some(contract_now.as_str());
        if version_ok && contract_ok {
            return Ok(());
        }
        warn!(
            chain_id = self.chain_id,
            version_from = version.as_deref().unwrap_or("<none>"),
            version_to = INDEXER_VERSION,
            contract_from = known_contract.as_deref().unwrap_or("<none>"),
            contract_to = %contract_now,
            "read-model shape or watched contract changed; clearing this chain for a full replay"
        );
        let mut tx = self.pool.begin().await?;
        for table in PROJECTION_TABLES {
            sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
                .bind(self.chain_id)
                .execute(&mut *tx)
                .await?;
        }
        // Everything the chain accumulated goes, by column, so a key added
        // later is cleared by default instead of surviving a replay because
        // this list forgot it. The one deliberate carve-out is the
        // deployment-block cache: it is keyed by contract, and eth_getCode
        // history does not change shape with the read model.
        sqlx::query(
            r#"DELETE FROM names.chain_metadata
               WHERE chain_id = $1 AND key NOT LIKE 'deploy_block:%'"#,
        )
        .bind(self.chain_id)
        .execute(&mut *tx)
        .await?;
        for (key, value) in [
            (SCHEMA_VERSION_KEY, INDEXER_VERSION.to_string()),
            (CONTRACT_KEY, contract_now),
        ] {
            sqlx::query(
                r#"INSERT INTO names.chain_metadata (chain_id, key, value)
                   VALUES ($1, $2, $3)
                   ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value"#,
            )
            .bind(self.chain_id)
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        info!(
            chain_id = self.chain_id,
            version = INDEXER_VERSION,
            "replay armed; starts next cycle"
        );
        Ok(())
    }

    /// Record how far the chain itself has grown; `/v1/status` reports the
    /// gap between this and the cursor as lag. Best effort, outside any
    /// window.
    pub async fn set_chain_head(&self, head: u64) {
        if let Err(e) = self.set_metadata(HEAD_KEY, &head.to_string()).await {
            warn!(%e, "failed to record the chain head");
        }
    }

    /// The chain head as last observed by the indexer loop.
    pub async fn chain_head(&self) -> Result<Option<u64>, sqlx::Error> {
        let value = self.get_metadata(HEAD_KEY).await?;
        Ok(value.and_then(|v| v.parse().ok()))
    }

    /// Record the block the cursor is actually chasing: the head minus the
    /// confirmation depth.
    ///
    /// Separate from [`Self::set_chain_head`] because the two answer different
    /// questions. The head is how far the CHAIN has grown, which is what
    /// `/v1/status` reports. This is how far the indexer INTENDS to get, and
    /// it is the only honest thing to measure a cursor against: the cursor is
    /// never advanced past it, so a fully caught-up mirror sits exactly here
    /// and `CONFIRMATIONS` blocks behind the head. Comparing the cursor to the
    /// head instead makes a healthy mirror look permanently late by the
    /// confirmation depth.
    ///
    /// Record the target, when it was reported, and how long readers may
    /// trust that report — `valid_for_secs` from now, by the database's
    /// clock. One statement, so the three cannot disagree: a report must
    /// never vouch for a target write that failed, and no host's clock
    /// enters into it.
    ///
    /// Returns whether the write landed, so a caller renewing the report
    /// later in the same cycle can decline to vouch for a target that never
    /// made it.
    pub async fn set_chain_target(&self, target: u64, valid_for_secs: u64) -> bool {
        let written = sqlx::query(
            r#"INSERT INTO names.chain_metadata (chain_id, key, value)
               VALUES ($1, $2, $3),
                      ($1, $4, EXTRACT(EPOCH FROM now())::bigint::text),
                      ($1, $5, (EXTRACT(EPOCH FROM now())::bigint + $6)::text)
               ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value"#,
        )
        .bind(self.chain_id)
        .bind(TARGET_KEY)
        .bind(target.to_string())
        .bind(TARGET_REPORTED_AT_KEY)
        .bind(TARGET_VALID_UNTIL_KEY)
        .bind(valid_for_bind(valid_for_secs))
        .execute(&self.pool)
        .await;
        match written {
            Ok(_) => true,
            Err(e) => {
                warn!(%e, "failed to record the chain target");
                false
            }
        }
    }

    /// Renew the report without moving the target. A long catch-up commits
    /// chunk by chunk under one target, and every committed chunk is proof
    /// that the loop is alive and the chain reachable; without this a healthy
    /// backfill would expire.
    pub async fn touch_chain_target(&self, valid_for_secs: u64) {
        let touched = sqlx::query(
            r#"INSERT INTO names.chain_metadata (chain_id, key, value)
               VALUES ($1, $2, EXTRACT(EPOCH FROM now())::bigint::text),
                      ($1, $3, (EXTRACT(EPOCH FROM now())::bigint + $4)::text)
               ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value"#,
        )
        .bind(self.chain_id)
        .bind(TARGET_REPORTED_AT_KEY)
        .bind(TARGET_VALID_UNTIL_KEY)
        .bind(valid_for_bind(valid_for_secs))
        .execute(&self.pool)
        .await;
        if let Err(e) = touched {
            warn!(%e, "failed to renew the chain target's report");
        }
    }

    /// Set when the report expires, as absolute Unix seconds.
    ///
    /// A test seam, and nothing else: production writes validity only through
    /// [`Self::set_chain_target`] and [`Self::touch_chain_target`], relative
    /// to the database's clock. Nothing in the binaries may call this.
    #[doc(hidden)]
    pub async fn set_chain_target_valid_until(&self, secs: u64) {
        if let Err(e) = self
            .set_metadata(TARGET_VALID_UNTIL_KEY, &secs.to_string())
            .await
        {
            warn!(%e, "failed to record when the chain target's report expires");
        }
    }

    /// Where the mirror stands, in one read: the target, the cursor, when the
    /// indexer last reported and how long that report is still good — the
    /// last by the database's clock, the same one that stamped it. One
    /// statement, so the four are a snapshot and one round trip on the
    /// gateway's hot path.
    pub async fn mirror_position(&self) -> Result<MirrorPosition, sqlx::Error> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            r#"SELECT key, value, EXTRACT(EPOCH FROM now())::bigint
               FROM names.chain_metadata
               WHERE chain_id = $1 AND key IN ($2, $3, $4, $5)"#,
        )
        .bind(self.chain_id)
        .bind(TARGET_KEY)
        .bind(CURSOR_KEY)
        .bind(TARGET_REPORTED_AT_KEY)
        .bind(TARGET_VALID_UNTIL_KEY)
        .fetch_all(&self.pool)
        .await?;
        let mut position = MirrorPosition::default();
        for (key, value, now) in rows {
            if key == TARGET_KEY {
                position.target = value.parse().ok();
            } else if key == CURSOR_KEY {
                position.cursor = value.parse().ok();
            } else if key == TARGET_REPORTED_AT_KEY {
                position.reported_at = value.parse().ok();
            } else if key == TARGET_VALID_UNTIL_KEY {
                position.valid_for = value
                    .parse::<i64>()
                    .ok()
                    .map(|until| until.saturating_sub(now));
            }
        }
        Ok(position)
    }

    /// The block the cursor is chasing, as last recorded by the indexer loop.
    ///
    /// `None` before the first window of a fresh deployment, and on an indexer
    /// old enough to predate the key — a caller that must not answer off a
    /// stale model treats both as "cannot tell".
    pub async fn chain_target(&self) -> Result<Option<u64>, sqlx::Error> {
        let value = self.get_metadata(TARGET_KEY).await?;
        Ok(value.and_then(|v| v.parse().ok()))
    }

    /// Record (or clear, with `None`) the last window failure, so a stalled
    /// loop is visible in `/v1/status` instead of only in the logs. Best
    /// effort.
    pub async fn set_window_error(&self, error: Option<&str>) {
        let result = match error {
            Some(msg) => self.set_metadata(WINDOW_ERROR_KEY, msg).await,
            None => sqlx::query(
                "DELETE FROM names.chain_metadata WHERE chain_id = $1 AND key = $2",
            )
            .bind(self.chain_id)
            .bind(WINDOW_ERROR_KEY)
            .execute(&self.pool)
            .await
            .map(|_| ()),
        };
        if let Err(e) = result {
            warn!(%e, "failed to record the window error state");
        }
    }

    /// The last window failure, if the most recent window failed.
    pub async fn window_error(&self) -> Result<Option<String>, sqlx::Error> {
        self.get_metadata(WINDOW_ERROR_KEY).await
    }

    /// The last fully-processed block, if any window ever committed.
    pub async fn cursor(&self) -> Result<Option<u64>, sqlx::Error> {
        let value = self.get_metadata(CURSOR_KEY).await?;
        Ok(value.and_then(|v| v.parse().ok()))
    }

    /// The cached deployment block for this contract on this chain, if
    /// detected before.
    pub async fn deploy_block(
        &self,
        contract: Address,
    ) -> Result<Option<u64>, sqlx::Error> {
        let value = self.get_metadata(&deploy_block_key(contract)).await?;
        Ok(value.and_then(|v| v.parse().ok()))
    }

    /// Cache a detected deployment block so a re-index does not redo the
    /// binary search.
    pub async fn set_deploy_block(
        &self,
        contract: Address,
        block: u64,
    ) -> Result<(), sqlx::Error> {
        self.set_metadata(&deploy_block_key(contract), &block.to_string())
            .await
    }

    /// Open a poll window. Everything applied through it commits atomically
    /// with the cursor when [`Window::commit`] runs, or not at all.
    pub async fn begin_window(&self) -> Result<Window, sqlx::Error> {
        Ok(Window {
            tx: self.pool.begin().await?,
            chain_id: self.chain_id,
        })
    }
}

/// Postgres TEXT and JSONB cannot hold a NUL byte, but the contract's
/// strings are byte-verbatim and an adversarial platform could emit one. A
/// lossy row beats a poison window that stalls the chain forever: the NUL is
/// replaced, the loss is logged, and the node columns — which is what the
/// chain actually keys — stay exact.
fn sanitize(value: &str, what: &str) -> String {
    if value.contains('\0') {
        error!(
            what,
            "string contains a NUL byte; storing with U+FFFD in its place"
        );
        value.replace('\0', "\u{fffd}")
    } else {
        value.to_string()
    }
}

/// Scrub NUL bytes anywhere in a JSON payload, reporting whether anything
/// was replaced — so the caller can log the loss as loudly as the projection
/// columns do, instead of the journal being scrubbed in silence.
fn sanitize_json(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) if s.contains('\0') => {
            *s = s.replace('\0', "\u{fffd}");
            true
        }
        serde_json::Value::String(_) => false,
        // Deliberately exhaustive: every element must be scrubbed, so no
        // short-circuiting combinator fits here.
        serde_json::Value::Array(items) => {
            let mut lossy = false;
            for item in items {
                lossy |= sanitize_json(item);
            }
            lossy
        }
        serde_json::Value::Object(map) => {
            let mut lossy = false;
            for item in map.values_mut() {
                lossy |= sanitize_json(item);
            }
            lossy
        }
        _ => false,
    }
}

fn as_i64(value: u64, what: &'static str) -> Result<i64, ApplyError> {
    i64::try_from(value).map_err(|_| ApplyError::OutOfRange { what, value })
}

/// One open poll window: a transaction that only knows how to apply events
/// and how to commit together with the cursor. That the cursor cannot be
/// left behind — or advanced without its writes — is not a convention here,
/// it is the shape of the API.
pub struct Window {
    tx: Transaction<'static, Postgres>,
    chain_id: i64,
}

impl Window {
    /// Advance the cursor and commit everything applied so far as one
    /// transaction: the cursor and the writes it stands for land together or
    /// not at all.
    pub async fn commit(mut self, through_block: u64) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO names.chain_metadata (chain_id, key, value)
               VALUES ($1, $2, $3)
               ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value"#,
        )
        .bind(self.chain_id)
        .bind(CURSOR_KEY)
        .bind(through_block.to_string())
        .execute(&mut *self.tx)
        .await?;
        self.tx.commit().await
    }

    /// Apply one decoded event: journal row plus projection writes, in the
    /// order the contract wrote its own storage. `Ok(false)` means the
    /// journal already held this (chain, block, log) — an earlier window
    /// applied it and committed — so the projections were left alone and the
    /// caller should not count it as new.
    pub async fn apply(
        &mut self,
        event: &NamesEvent,
        pos: &LogPosition,
    ) -> Result<bool, ApplyError> {
        let chain_id = self.chain_id;
        let block = as_i64(pos.block_number, "block number")?;
        let log_index = as_i64(pos.log_index, "log index")?;
        let tx = &mut self.tx;

        let mut payload = event.payload();
        if sanitize_json(&mut payload) {
            error!(
                kind = event.kind(),
                "journal payload contained a NUL byte; stored with U+FFFD in its place"
            );
        }
        let journaled = sqlx::query(
            r#"INSERT INTO names.events (chain_id, block_number, log_index, tx_hash, kind, payload)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (chain_id, block_number, log_index) DO NOTHING"#,
        )
        .bind(chain_id)
        .bind(block)
        .bind(log_index)
        .bind(pos.tx_hash.as_slice())
        .bind(event.kind())
        .bind(payload)
        .execute(&mut **tx)
        .await?;
        if journaled.rows_affected() == 0 {
            // Running the projection writes again would be harmless for the
            // upserts but would double-count `configured_count`, so the
            // journal's conflict is the one idempotency gate for everything.
            return Ok(false);
        }

        match event {
            NamesEvent::IdentityBound {
                owner,
                id_node,
                handle_node,
                platform_id,
                user_id,
                handle,
                observed_at,
                published,
                version,
            } => {
                // Self-check: this build's constants against the chain's
                // topics. The emitted nodes win either way — the chain
                // already keyed its storage by them — but a mismatch means
                // platform ids or hashing drifted and resolution-by-string is
                // broken until fixed.
                if nodes::id_node(*platform_id, user_id) != *id_node {
                    error!(%id_node, user_id, "recomputed idNode disagrees with the emitted topic");
                }
                if nodes::handle_node(
                    *platform_id,
                    &nodes::NormalizedHandle::from_chain(handle),
                ) != *handle_node
                {
                    error!(%handle_node, handle, "recomputed handleNode disagrees with the emitted topic");
                }

                let observed = as_i64(*observed_at, "observedAt")?;
                let user_id = sanitize(user_id, "userId");
                let handle = sanitize(handle, "handle");
                sqlx::query(
                    r#"INSERT INTO names.ids
                           (chain_id, id_node, platform_id, user_id, owner,
                            observed_at, version, handle_node, block_number, log_index)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                       ON CONFLICT (chain_id, id_node) DO UPDATE SET
                           owner = EXCLUDED.owner,
                           observed_at = EXCLUDED.observed_at,
                           version = EXCLUDED.version,
                           handle_node = EXCLUDED.handle_node,
                           block_number = EXCLUDED.block_number,
                           log_index = EXCLUDED.log_index,
                           updated_at = now()"#,
                )
                .bind(chain_id)
                .bind(id_node.as_slice())
                .bind(platform_id.as_slice())
                .bind(&user_id)
                .bind(owner.as_slice())
                .bind(observed)
                .bind(i64::from(*version))
                .bind(handle_node.as_slice())
                .bind(block)
                .bind(log_index)
                .execute(&mut **tx)
                .await?;

                sqlx::query(
                    r#"INSERT INTO names.handles
                           (chain_id, handle_node, platform_id, handle, owner,
                            observed_at, version, id_node, block_number, log_index)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                       ON CONFLICT (chain_id, handle_node) DO UPDATE SET
                           handle = EXCLUDED.handle,
                           owner = EXCLUDED.owner,
                           observed_at = EXCLUDED.observed_at,
                           version = EXCLUDED.version,
                           id_node = EXCLUDED.id_node,
                           block_number = EXCLUDED.block_number,
                           log_index = EXCLUDED.log_index,
                           updated_at = now()"#,
                )
                .bind(chain_id)
                .bind(handle_node.as_slice())
                .bind(platform_id.as_slice())
                .bind(&handle)
                .bind(owner.as_slice())
                .bind(observed)
                .bind(i64::from(*version))
                .bind(id_node.as_slice())
                .bind(block)
                .bind(log_index)
                .execute(&mut **tx)
                .await?;

                // The event's flag is the post-state of the contract's
                // published string: true upserts, false deletes. Deleting on
                // false is what makes a replay converge — the contract's
                // bind() refreshes an existing publication even when the
                // caller passed false, and the flag already accounts for
                // that.
                if *published {
                    sqlx::query(
                        r#"INSERT INTO names.published (chain_id, owner, platform_id, handle)
                           VALUES ($1, $2, $3, $4)
                           ON CONFLICT (chain_id, owner, platform_id) DO UPDATE SET
                               handle = EXCLUDED.handle,
                               updated_at = now()"#,
                    )
                    .bind(chain_id)
                    .bind(owner.as_slice())
                    .bind(platform_id.as_slice())
                    .bind(&handle)
                    .execute(&mut **tx)
                    .await?;
                } else {
                    sqlx::query(
                        r#"DELETE FROM names.published
                           WHERE chain_id = $1 AND owner = $2 AND platform_id = $3"#,
                    )
                    .bind(chain_id)
                    .bind(owner.as_slice())
                    .bind(platform_id.as_slice())
                    .execute(&mut **tx)
                    .await?;
                }
            }

            NamesEvent::HandleRetired {
                platform_id: _,
                handle_node,
                owner: _,
            } => {
                // Mirror the contract exactly: owner and version zero out,
                // the observed-at watermark and the id back-pointer stay.
                sqlx::query(
                    r#"UPDATE names.handles SET
                           owner = NULL,
                           version = 0,
                           block_number = $3,
                           log_index = $4,
                           updated_at = now()
                       WHERE chain_id = $1 AND handle_node = $2"#,
                )
                .bind(chain_id)
                .bind(handle_node.as_slice())
                .bind(block)
                .bind(log_index)
                .execute(&mut **tx)
                .await?;
            }

            NamesEvent::NameUnpublished { owner, platform_id } => {
                sqlx::query(
                    r#"DELETE FROM names.published
                       WHERE chain_id = $1 AND owner = $2 AND platform_id = $3"#,
                )
                .bind(chain_id)
                .bind(owner.as_slice())
                .bind(platform_id.as_slice())
                .execute(&mut **tx)
                .await?;
            }

            NamesEvent::PlatformConfigured { platform_id } => {
                let key = nodes::Platform::key_of(*platform_id);
                let reconfigured: bool = sqlx::query_scalar(
                    r#"INSERT INTO names.platforms
                           (chain_id, platform_id, platform_key, configured_count, last_configured_block)
                       VALUES ($1, $2, $3, 1, $4)
                       ON CONFLICT (chain_id, platform_id) DO UPDATE SET
                           platform_key = EXCLUDED.platform_key,
                           configured_count = names.platforms.configured_count + 1,
                           last_configured_block = EXCLUDED.last_configured_block,
                           updated_at = now()
                       RETURNING configured_count > 1"#,
                )
                .bind(chain_id)
                .bind(platform_id.as_slice())
                .bind(key)
                .bind(block)
                .fetch_one(&mut **tx)
                .await?;
                if reconfigured {
                    // setPlatform ran again. If the rules changed, every
                    // handle node on the platform is re-keyed on chain, and
                    // this build's compiled-in rules may now normalize
                    // differently than the contract. The event carries no
                    // rules payload, so the honest move is to say so loudly
                    // and keep indexing by node — node lookups stay exact
                    // either way.
                    warn!(
                        %platform_id,
                        block,
                        "platform reconfigured on chain; if its rules changed, \
                         string lookups need this build's rules updated and a re-index"
                    );
                }
            }

            NamesEvent::VerifierConfigured {
                platform_id,
                version,
                verifier,
                max_future_observation,
            } => {
                sqlx::query(
                    r#"INSERT INTO names.verifiers
                           (chain_id, platform_id, version, verifier, max_future_observation, retired)
                       VALUES ($1, $2, $3, $4, $5, false)
                       ON CONFLICT (chain_id, platform_id, version) DO UPDATE SET
                           verifier = EXCLUDED.verifier,
                           max_future_observation = EXCLUDED.max_future_observation,
                           retired = false,
                           updated_at = now()"#,
                )
                .bind(chain_id)
                .bind(platform_id.as_slice())
                .bind(i64::from(*version))
                .bind(verifier.as_slice())
                .bind(as_i64(*max_future_observation, "maxFutureObservation")?)
                .execute(&mut **tx)
                .await?;
            }

            NamesEvent::VerifierRetired {
                platform_id,
                version,
            } => {
                sqlx::query(
                    r#"UPDATE names.verifiers SET retired = true, updated_at = now()
                       WHERE chain_id = $1 AND platform_id = $2 AND version = $3"#,
                )
                .bind(chain_id)
                .bind(platform_id.as_slice())
                .bind(i64::from(*version))
                .execute(&mut **tx)
                .await?;
            }

            NamesEvent::LatestVersionChanged {
                platform_id,
                version,
            } => {
                let key = nodes::Platform::key_of(*platform_id);
                sqlx::query(
                    r#"INSERT INTO names.platforms
                           (chain_id, platform_id, platform_key, latest_version)
                       VALUES ($1, $2, $3, $4)
                       ON CONFLICT (chain_id, platform_id) DO UPDATE SET
                           latest_version = EXCLUDED.latest_version,
                           updated_at = now()"#,
                )
                .bind(chain_id)
                .bind(platform_id.as_slice())
                .bind(key)
                .bind(i64::from(*version))
                .execute(&mut **tx)
                .await?;
            }
        }

        Ok(true)
    }
}

/// The projection every `names.ids` read shares: the row itself, the handle it
/// points at, and whether that pair is displayed.
///
/// Written once because it was written three times. The joins encode which
/// columns of which table a row is assembled from, so a change to
/// `names.published` — the one most likely to move — has to reach every reader
/// or the ones it missed answer from a shape that no longer exists. A caller
/// appends its own `WHERE`, which is the only part that actually differs.
const IDENTITY_PROJECTION: &str = r#"SELECT i.platform_id, i.user_id, i.id_node, i.owner,
                      i.observed_at, i.version, i.handle_node,
                      h.handle, h.owner AS handle_owner, h.id_node AS handle_id_node,
                      (p.handle IS NOT NULL) AS published
               FROM names.ids i
               LEFT JOIN names.handles h
                 ON h.chain_id = i.chain_id AND h.handle_node = i.handle_node
               LEFT JOIN names.published p
                 ON p.chain_id = i.chain_id AND p.owner = i.owner
                    AND p.platform_id = i.platform_id AND p.handle = h.handle
               "#;

// ─── The read side ──────────────────────────────────────────────────────────
// The same store the writer uses answers the API's queries, so all SQL —
// and the join shapes the projections were designed for — lives in one
// module. Handlers translate HTTP to these calls and rows to JSON; they do
// not own queries.

/// One `names.handles` row joined with the id it points back at: everything
/// `resolveHandle` answers from.
#[derive(sqlx::FromRow)]
pub struct HandleRow {
    /// The normalized handle, as the chain emitted it.
    pub handle: String,
    /// The storage key the chain filed this handle under.
    pub handle_node: Vec<u8>,
    /// The wallet, or `None` after retirement — the contract's `address(0)`.
    pub owner: Option<Vec<u8>>,
    /// The proof-freshness watermark, kept even through retirement.
    pub observed_at: i64,
    /// The proof version; 0 after retirement, like the contract.
    pub version: i64,
    /// The account id node this handle points back at (`idOfHandle`).
    pub id_node: Vec<u8>,
    /// The plaintext account id behind that node, when it was ever bound.
    pub user_id: Option<String>,
    /// The wallet that account id currently resolves to.
    pub id_owner: Option<Vec<u8>>,
}

impl HandleRow {
    /// Mirrors `resolvePair`: the account id this handle points back at
    /// still resolves to the same wallet.
    pub fn id_agrees(&self) -> bool {
        match &self.owner {
            Some(owner) => self.id_owner.as_deref() == Some(owner.as_slice()),
            None => false,
        }
    }
}

/// One `names.ids` row joined with the handle node it points at and the
/// published mapping: everything `resolveId` and the reverse display answer
/// from.
#[derive(sqlx::FromRow)]
pub struct IdentityRow {
    /// The platform the account lives on.
    pub platform_id: Vec<u8>,
    /// The plaintext account id, byte-verbatim as the chain keys it.
    pub user_id: String,
    /// The storage key the chain filed this account under.
    pub id_node: Vec<u8>,
    /// The wallet that proved the account.
    pub owner: Vec<u8>,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The proof version.
    pub version: i64,
    /// The handle node this account last proved (`handleOfId`).
    pub handle_node: Vec<u8>,
    /// The handle string at that node, when the node was ever bound.
    pub handle: Option<String>,
    /// The wallet the handle node currently resolves to.
    pub handle_owner: Option<Vec<u8>>,
    /// The id node the handle currently points back at.
    pub handle_id_node: Option<Vec<u8>>,
    /// Whether a published row exists for (owner, platform, handle).
    pub published: bool,
}

impl IdentityRow {
    /// The `handleOfId`/`idOfHandle` round trip, written once: the stored
    /// handle string is presentable only while the node still points back at
    /// this id and still resolves to this wallet. Otherwise the account
    /// renamed or was overtaken, and showing the stale string would
    /// mis-route a payment.
    pub fn handle_still_owned(&self) -> bool {
        self.handle_owner.as_deref() == Some(self.owner.as_slice())
            && self.handle_id_node.as_deref() == Some(self.id_node.as_slice())
    }

    /// The handle to present, when it is still this account's.
    pub fn presentable_handle(&self) -> Option<&str> {
        if self.handle_still_owned() {
            self.handle.as_deref()
        } else {
            None
        }
    }

    /// Whether this is the wallet's displayed name on the platform — only
    /// while the handle is still the account's.
    pub fn displayed(&self) -> bool {
        self.published && self.handle_still_owned()
    }
}

/// One search hit: a live handle, its owner, and the id it pairs with.
#[derive(sqlx::FromRow)]
pub struct SearchRow {
    /// The platform the handle lives on.
    pub platform_id: Vec<u8>,
    /// The normalized handle.
    pub handle: String,
    /// The wallet it resolves to (retired handles never match).
    pub owner: Vec<u8>,
    /// The account id behind it, when bound.
    pub user_id: Option<String>,
    /// Whether the owner displays this handle.
    pub published: bool,
}

/// Escape LIKE metacharacters so query text matches itself.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl ChainStore {
    /// Whether the chain ever configured this platform — the difference
    /// between the contract's `UnknownPlatform` revert and its zero-address
    /// answer.
    pub async fn platform_wired(&self, platform_id: B256) -> Result<bool, sqlx::Error> {
        // EXISTS, not `SELECT 1`: a bare literal is INT4 on the wire, and
        // decoding it as i64 fails exactly when a row IS found — so the
        // check passed for unconfigured platforms and 500'd for configured
        // ones, which is how it escaped every absent-platform test.
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM names.platforms
              WHERE chain_id = $1 AND platform_id = $2)",
        )
        .bind(self.chain_id)
        .bind(platform_id.as_slice())
        .fetch_one(&self.pool)
        .await
    }

    /// The row `resolveHandle` answers from, by the platform and the
    /// chain-normalized handle. Demanding [`NormalizedHandle`] keeps a raw
    /// query string — which would never match a stored row — out of the SQL.
    pub async fn resolve_handle(
        &self,
        platform_id: B256,
        handle: &NormalizedHandle,
    ) -> Result<Option<HandleRow>, sqlx::Error> {
        sqlx::query_as(
            r#"SELECT h.handle, h.handle_node, h.owner, h.observed_at,
                      h.version, h.id_node,
                      i.user_id, i.owner AS id_owner
               FROM names.handles h
               LEFT JOIN names.ids i
                 ON i.chain_id = h.chain_id AND i.id_node = h.id_node
               WHERE h.chain_id = $1 AND h.platform_id = $2 AND h.handle = $3"#,
        )
        .bind(self.chain_id)
        .bind(platform_id.as_slice())
        .bind(handle.as_str())
        .fetch_optional(&self.pool)
        .await
    }

    /// The row `resolveId` answers from. The id is matched byte-verbatim,
    /// exactly as the chain keys it.
    pub async fn resolve_id(
        &self,
        platform_id: B256,
        user_id: &str,
    ) -> Result<Option<IdentityRow>, sqlx::Error> {
        sqlx::query_as(
            &format!(
            "{IDENTITY_PROJECTION}WHERE i.chain_id = $1 AND i.platform_id = $2 AND i.user_id = $3"
        ),
        )
        .bind(self.chain_id)
        .bind(platform_id.as_slice())
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Every identity a wallet proved — `primaryOf`'s reverse display,
    /// platform by platform.
    pub async fn identities_of(
        &self,
        owner: Address,
    ) -> Result<Vec<IdentityRow>, sqlx::Error> {
        sqlx::query_as(
            &format!(
            "{IDENTITY_PROJECTION}WHERE i.chain_id = $1 AND i.owner = $2\n               ORDER BY i.platform_id, i.user_id"
        ),
        )
        .bind(self.chain_id)
        .bind(owner.as_slice())
        .fetch_all(&self.pool)
        .await
    }

    /// Live handles matching a folded partial query: exact first, then
    /// prefix, then substring, then trigram-fuzzy. LIKE-escaping is this
    /// method's problem, not the caller's — it exists so the query text
    /// matches itself, which is SQL knowledge.
    pub async fn search_handles(
        &self,
        platform_id: Option<B256>,
        folded_query: &str,
        limit: i64,
    ) -> Result<Vec<SearchRow>, sqlx::Error> {
        let like = escape_like(folded_query);
        sqlx::query_as(
            r#"SELECT h.platform_id, h.handle, h.owner, i.user_id,
                      (p.handle IS NOT NULL) AS published
               FROM names.handles h
               LEFT JOIN names.ids i
                 ON i.chain_id = h.chain_id AND i.id_node = h.id_node
               LEFT JOIN names.published p
                 ON p.chain_id = h.chain_id AND p.owner = h.owner
                    AND p.platform_id = h.platform_id AND p.handle = h.handle
               WHERE h.chain_id = $1
                 AND h.owner IS NOT NULL
                 AND ($2::bytea IS NULL OR h.platform_id = $2)
                 AND (h.handle LIKE '%' || $3 || '%' ESCAPE '\'
                      OR h.handle % $4)
               ORDER BY (h.handle = $4) DESC,
                        (h.handle LIKE $3 || '%' ESCAPE '\') DESC,
                        (h.handle LIKE '%' || $3 || '%' ESCAPE '\') DESC,
                        similarity(h.handle, $4) DESC,
                        h.handle ASC
               LIMIT $5"#,
        )
        .bind(self.chain_id)
        .bind(platform_id.as_ref().map(|p| p.as_slice().to_vec()))
        .bind(&like)
        .bind(folded_query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }
}
