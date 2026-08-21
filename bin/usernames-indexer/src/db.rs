//! Postgres: pool setup, the metadata keys, and the event apply path.
//!
//! Every write in [`apply`] is an idempotent upsert keyed by what the contract
//! keys its own storage by, so replaying a window — after a crash, or after a
//! version-bump re-index — converges instead of duplicating.

use alloy::primitives::Address;
use sqlx::{
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
    nodes,
};

/// Bump on any change to what the indexer writes. A mismatch at startup
/// clears the chain's rows and cursor, so the next loop replays the chain
/// from the deployment block — the re-index IS the migration.
pub const INDEXER_VERSION: &str = "1";

fn schema_version_key(chain_id: i64) -> String {
    format!("indexer_schema_version:{chain_id}")
}

fn contract_key(chain_id: i64) -> String {
    format!("contract:{chain_id}")
}

fn cursor_key(chain_id: i64) -> String {
    format!("indexer_last_block:{chain_id}")
}

fn head_key(chain_id: i64) -> String {
    format!("chain_head:{chain_id}")
}

fn window_error_key(chain_id: i64) -> String {
    format!("last_window_error:{chain_id}")
}

fn deploy_block_key(chain_id: i64, contract: Address) -> String {
    // The address is part of the key: repointing the indexer at a different
    // contract must not inherit the old contract's deployment block.
    format!("deploy_block:{chain_id}:{contract}")
}

/// Connect and bring the schema current.
pub async fn connect_and_migrate(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPool::connect(database_url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

async fn get_metadata(pool: &PgPool, key: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT value FROM names.pipeline_metadata WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
}

async fn set_metadata(pool: &PgPool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO names.pipeline_metadata (key, value) VALUES ($1, $2)
           ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"#,
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// A held per-chain writer lease. One process indexes one chain at a time:
/// the Postgres advisory lock lives on this dedicated connection for the
/// process's lifetime, so a second instance — a rolling-deploy overlap, a
/// stray operator shell — blocks instead of interleaving writes with ours.
pub struct ChainLock {
    _conn: sqlx::pool::PoolConnection<Postgres>,
}

/// Take the chain's writer lease, waiting (with a log line) if another
/// instance still holds it.
pub async fn acquire_chain_lock(
    pool: &PgPool,
    chain_id: i64,
) -> Result<ChainLock, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let key = format!("usernames-indexer:{chain_id}");
    let taken: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
            .bind(&key)
            .fetch_one(&mut *conn)
            .await?;
    if !taken {
        warn!(
            chain_id,
            "another indexer holds this chain's writer lock; waiting"
        );
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(&key)
            .execute(&mut *conn)
            .await?;
    }
    Ok(ChainLock { _conn: conn })
}

/// Clear and rescan this chain when this build writes a different shape than
/// the database holds — or when the watched contract changed, because the old
/// contract's bindings are not this contract's bindings. Strictly scoped to
/// `chain_id`: a co-tenant chain's rows, cursor and metadata are untouched.
/// The deployment-block cache survives a version bump (it is keyed by
/// contract, and `eth_getCode` history does not change shape with the read
/// model).
///
/// Call with the chain's [`ChainLock`] held: the lock is what makes the
/// check-clear-stamp sequence safe against a still-running older instance.
pub async fn prepare_chain(
    pool: &PgPool,
    chain_id: i64,
    contract: Address,
) -> Result<(), sqlx::Error> {
    let version = get_metadata(pool, &schema_version_key(chain_id)).await?;
    let known_contract = get_metadata(pool, &contract_key(chain_id)).await?;
    let contract_now = contract.to_string().to_lowercase();
    let version_ok = version.as_deref() == Some(INDEXER_VERSION);
    let contract_ok = known_contract.as_deref() == Some(contract_now.as_str());
    if version_ok && contract_ok {
        return Ok(());
    }
    warn!(
        chain_id,
        version_from = version.as_deref().unwrap_or("<none>"),
        version_to = INDEXER_VERSION,
        contract_from = known_contract.as_deref().unwrap_or("<none>"),
        contract_to = %contract_now,
        "read-model shape or watched contract changed; clearing this chain for a full replay"
    );
    let mut tx = pool.begin().await?;
    for table in [
        "events",
        "ids",
        "handles",
        "published",
        "platforms",
        "verifiers",
    ] {
        sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
            .bind(chain_id)
            .execute(&mut *tx)
            .await?;
    }
    for key in [
        cursor_key(chain_id),
        head_key(chain_id),
        window_error_key(chain_id),
    ] {
        sqlx::query("DELETE FROM names.pipeline_metadata WHERE key = $1")
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    for (key, value) in [
        (schema_version_key(chain_id), INDEXER_VERSION.to_string()),
        (contract_key(chain_id), contract_now),
    ] {
        sqlx::query(
            r#"INSERT INTO names.pipeline_metadata (key, value) VALUES ($1, $2)
               ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"#,
        )
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    info!(
        chain_id,
        version = INDEXER_VERSION,
        "replay armed; starts next cycle"
    );
    Ok(())
}

/// Record how far the chain itself has grown; `/v1/status` reports the gap
/// between this and the cursor as lag. Best effort, outside any window.
pub async fn set_chain_head(pool: &PgPool, chain_id: i64, head: u64) {
    if let Err(e) = set_metadata(pool, &head_key(chain_id), &head.to_string()).await {
        warn!(%e, "failed to record the chain head");
    }
}

/// The chain head as last observed by the indexer loop.
pub async fn get_chain_head(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Option<u64>, sqlx::Error> {
    let value = get_metadata(pool, &head_key(chain_id)).await?;
    Ok(value.and_then(|v| v.parse().ok()))
}

/// Record (or clear, with `None`) the last window failure, so a stalled loop
/// is visible in `/v1/status` instead of only in the logs. Best effort.
pub async fn set_window_error(pool: &PgPool, chain_id: i64, error: Option<&str>) {
    let result = match error {
        Some(msg) => set_metadata(pool, &window_error_key(chain_id), msg).await,
        None => sqlx::query("DELETE FROM names.pipeline_metadata WHERE key = $1")
            .bind(window_error_key(chain_id))
            .execute(pool)
            .await
            .map(|_| ()),
    };
    if let Err(e) = result {
        warn!(%e, "failed to record the window error state");
    }
}

/// The last window failure, if the most recent window failed.
pub async fn get_window_error(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    get_metadata(pool, &window_error_key(chain_id)).await
}

/// The last fully-processed block, if any window ever committed.
pub async fn get_cursor(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Option<u64>, sqlx::Error> {
    let value = get_metadata(pool, &cursor_key(chain_id)).await?;
    Ok(value.and_then(|v| v.parse().ok()))
}

/// Advance the cursor inside the window's transaction: the cursor and the
/// writes it stands for commit together or not at all.
pub async fn set_cursor(
    tx: &mut Transaction<'_, Postgres>,
    chain_id: i64,
    block: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO names.pipeline_metadata (key, value) VALUES ($1, $2)
           ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"#,
    )
    .bind(cursor_key(chain_id))
    .bind(block.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The cached deployment block for this (chain, contract), if detected before.
pub async fn get_deploy_block(
    pool: &PgPool,
    chain_id: i64,
    contract: Address,
) -> Result<Option<u64>, sqlx::Error> {
    let value = get_metadata(pool, &deploy_block_key(chain_id, contract)).await?;
    Ok(value.and_then(|v| v.parse().ok()))
}

/// Cache a detected deployment block so a re-index does not redo the binary
/// search.
pub async fn set_deploy_block(
    pool: &PgPool,
    chain_id: i64,
    contract: Address,
    block: u64,
) -> Result<(), sqlx::Error> {
    set_metadata(
        pool,
        &deploy_block_key(chain_id, contract),
        &block.to_string(),
    )
    .await
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

fn sanitize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if s.contains('\0') {
                *s = s.replace('\0', "\u{fffd}");
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sanitize_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(sanitize_json),
        _ => {}
    }
}

fn as_i64(value: u64, what: &str) -> Result<i64, sqlx::Error> {
    i64::try_from(value).map_err(|_| {
        sqlx::Error::Protocol(format!("{what} {value} does not fit in a BIGINT"))
    })
}

/// Apply one decoded event inside the window's transaction: journal row plus
/// projection writes, in the order the contract wrote its own storage.
pub async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    chain_id: i64,
    event: &NamesEvent,
    pos: &LogPosition,
) -> Result<(), sqlx::Error> {
    let block = as_i64(pos.block_number, "block number")?;
    let log_index = as_i64(pos.log_index, "log index")?;

    let mut payload = event.payload();
    sanitize_json(&mut payload);
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
        // The journal already holds this (chain, block, log) — some earlier
        // window applied it and committed. Running the projection writes
        // again would be harmless for the upserts but would double-count
        // `configured_count`, so the journal's conflict is the one
        // idempotency gate for everything.
        return Ok(());
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
            // Self-check: this build's constants against the chain's topics.
            // The emitted nodes win either way — the chain already keyed its
            // storage by them — but a mismatch means platform ids or hashing
            // drifted and resolution-by-string is broken until fixed.
            if nodes::id_node(*platform_id, user_id) != *id_node {
                error!(%id_node, user_id, "recomputed idNode disagrees with the emitted topic");
            }
            if nodes::handle_node(*platform_id, handle) != *handle_node {
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

            // The event's flag is the post-state of the contract's published
            // string: true upserts, false deletes. Deleting on false is what
            // makes a replay converge — the contract's bind() refreshes an
            // existing publication even when the caller passed false, and the
            // flag already accounts for that.
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
            // Mirror the contract exactly: owner and version zero out, the
            // observed-at watermark and the id back-pointer stay.
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
            let key = nodes::KNOWN_PLATFORMS.get(platform_id).copied();
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
                // setPlatform ran again. If the rules changed, every handle
                // node on the platform is re-keyed on chain, and this build's
                // compiled-in rules may now normalize differently than the
                // contract. The event carries no rules payload, so the honest
                // move is to say so loudly and keep indexing by node — node
                // lookups stay exact either way.
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
            let key = nodes::KNOWN_PLATFORMS.get(platform_id).copied();
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

    Ok(())
}
