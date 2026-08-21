//! The read model, exercised end to end: decoded events go through the same
//! apply path the indexer uses, and the assertions go through the same axum
//! handlers a caller would hit.
//!
//! Tests skip silently when `DATABASE_URL` is unset — that is the only
//! legitimate reason. Everything past that check panics on failure, so a
//! broken migration cannot masquerade as "no database around".

use alloy::primitives::{
    Address,
    B256,
};
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tokio::sync::{
    Mutex,
    MutexGuard,
};
use tower::ServiceExt;
use usernames_indexer::{
    api,
    db,
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes,
};

/// One chain id for every test; arbitrary, only consistency matters.
const CHAIN: i64 = 31337;

static DB_LOCK: Mutex<()> = Mutex::const_new(());

async fn test_pool() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let guard = DB_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("DATABASE_URL is set but connecting failed");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations failed");
    for table in [
        "events",
        "ids",
        "handles",
        "published",
        "platforms",
        "verifiers",
    ] {
        sqlx::query(&format!("TRUNCATE names.{table}"))
            .execute(&pool)
            .await
            .expect("truncate failed");
    }
    sqlx::query("DELETE FROM names.pipeline_metadata")
        .execute(&pool)
        .await
        .expect("metadata cleanup failed");
    Some((pool, guard))
}

fn addr(byte: u8) -> Address {
    Address::from_slice(&[byte; 20])
}

/// Apply one event at a synthetic chain position, committing like a window.
async fn apply(pool: &PgPool, seq: u64, event: NamesEvent) {
    let pos = LogPosition {
        block_number: seq,
        log_index: 0,
        tx_hash: B256::from_slice(&[seq as u8; 32]),
    };
    let mut tx = pool.begin().await.expect("begin");
    db::apply(&mut tx, CHAIN, &event, &pos)
        .await
        .expect("apply");
    // Cursor in the same transaction, exactly as the loop commits — which is
    // also what lets the sync-gated API answer.
    db::set_cursor(&mut tx, CHAIN, seq).await.expect("cursor");
    tx.commit().await.expect("commit");
}

fn bind(
    owner: Address,
    platform: B256,
    user_id: &str,
    handle: &str,
    observed_at: u64,
    published: bool,
) -> NamesEvent {
    NamesEvent::IdentityBound {
        owner,
        id_node: nodes::id_node(platform, user_id),
        handle_node: nodes::handle_node(platform, handle),
        platform_id: platform,
        user_id: user_id.into(),
        handle: handle.into(),
        observed_at,
        published,
        version: 1,
    }
}

fn retire(platform: B256, handle: &str, owner: Address) -> NamesEvent {
    NamesEvent::HandleRetired {
        platform_id: platform,
        handle_node: nodes::handle_node(platform, handle),
        owner,
    }
}

async fn get(pool: &PgPool, path: &str) -> (StatusCode, serde_json::Value) {
    let state = api::AppState {
        pool: pool.clone(),
        chain_id: CHAIN,
        contract: addr(0xCC),
    };
    let response = api::router(state)
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}

#[tokio::test]
async fn bind_resolves_all_three_directions() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    let alice = addr(0xA1);
    apply(&pool, 1, bind(alice, x, "111", "alice_1", 1000, true)).await;

    let (status, body) = get(&pool, "/v1/resolve/handle/x/alice_1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], alice.to_string().as_str());
    assert_eq!(body["userId"], "111");
    assert_eq!(body["idAgrees"], true);

    // The path normalizes the way the chain did: raw form finds the same row.
    let (status, body) = get(&pool, "/v1/resolve/handle/x/@Alice_1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["handle"], "alice_1");

    let (status, body) = get(&pool, "/v1/resolve/id/x/111").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], alice.to_string().as_str());
    assert_eq!(body["handle"], "alice_1");
    assert_eq!(body["published"], true);

    let (status, body) = get(&pool, &format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["identities"][0]["handle"], "alice_1");
    assert_eq!(body["identities"][0]["published"], true);
    assert_eq!(body["identities"][0]["resolves"], true);
}

#[tokio::test]
async fn rename_retires_the_previous_handle() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    let alice = addr(0xA1);
    apply(&pool, 1, bind(alice, x, "111", "alice_1", 1000, true)).await;
    // The contract emits the retirement before the new bind in the same tx.
    apply(&pool, 2, retire(x, "alice_1", alice)).await;
    apply(&pool, 3, bind(alice, x, "111", "alice_2", 2000, true)).await;

    let (status, body) = get(&pool, "/v1/resolve/handle/x/alice_1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = get(&pool, "/v1/resolve/handle/x/alice_2").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], alice.to_string().as_str());

    let (status, body) = get(&pool, "/v1/resolve/id/x/111").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["handle"], "alice_2");

    // The retired node keeps its watermark, mirroring the contract's
    // stale-proof gate.
    let watermark: i64 = sqlx::query_scalar(
        "SELECT observed_at FROM names.handles WHERE chain_id = $1 AND handle = 'alice_1'",
    )
    .bind(CHAIN)
    .fetch_one(&pool)
    .await
    .expect("watermark row");
    assert_eq!(watermark, 1000);
}

#[tokio::test]
async fn takeover_repoints_the_handle_and_orphans_the_old_id() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    let (alice, bob) = (addr(0xA1), addr(0xB2));
    // Alice holds the handle, renames her platform account, re-proves under
    // the same handle... then the platform recycles the name to Bob's
    // account, and Bob proves it with a fresher observation.
    apply(&pool, 1, bind(alice, x, "111", "popular", 1000, false)).await;
    apply(&pool, 2, bind(bob, x, "222", "popular", 2000, false)).await;

    // The handle resolves to Bob now, and idAgrees pairs it with Bob's id.
    let (status, body) = get(&pool, "/v1/resolve/handle/x/popular").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], bob.to_string().as_str());
    assert_eq!(body["userId"], "222");
    assert_eq!(body["idAgrees"], true);

    // Alice's id still resolves to her wallet, but the handle is no longer
    // hers to display: the node points at Bob's id.
    let (status, body) = get(&pool, "/v1/resolve/id/x/111").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], alice.to_string().as_str());
    assert_eq!(body["handle"], serde_json::Value::Null);
}

#[tokio::test]
async fn publish_flag_is_the_post_state() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    let alice = addr(0xA1);
    apply(&pool, 1, bind(alice, x, "111", "alice_1", 1000, true)).await;
    // A later bind that reports published=false clears the display name.
    apply(&pool, 2, retire(x, "alice_1", alice)).await;
    apply(&pool, 3, bind(alice, x, "111", "alice_2", 2000, false)).await;

    let (status, body) = get(&pool, &format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["identities"][0]["published"], false);

    // And NameUnpublished after a published bind does the same.
    apply(&pool, 4, retire(x, "alice_2", alice)).await;
    apply(&pool, 5, bind(alice, x, "111", "alice_3", 3000, true)).await;
    apply(
        &pool,
        6,
        NamesEvent::NameUnpublished {
            owner: alice,
            platform_id: x,
        },
    )
    .await;
    let (_, body) = get(&pool, &format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(body["identities"][0]["published"], false);
}

#[tokio::test]
async fn search_ranks_exact_prefix_substring() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    let github = nodes::platform_id_for_key("github").unwrap();
    apply(&pool, 1, bind(addr(1), x, "1", "ali", 1000, false)).await;
    apply(&pool, 2, bind(addr(2), x, "2", "alice_1", 1000, true)).await;
    apply(&pool, 3, bind(addr(3), x, "3", "malice", 1000, false)).await;
    apply(&pool, 4, bind(addr(4), x, "4", "bob", 1000, false)).await;
    apply(&pool, 5, bind(addr(5), github, "5", "alicorn", 1000, false)).await;

    let (status, body) = get(&pool, "/v1/search?q=ali").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let handles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["handle"].as_str().unwrap())
        .collect();
    // Exact first, then prefix alphabetically, then substring.
    assert_eq!(handles, ["ali", "alice_1", "alicorn", "malice"]);
    // And the hit carries the right binding, not just the right string.
    let exact = &body["hits"][0];
    assert_eq!(exact["owner"], addr(1).to_string().as_str(), "{body}");
    assert_eq!(exact["userId"], "1", "{body}");
    assert_eq!(exact["published"], false, "{body}");
    assert_eq!(body["hits"][1]["published"], true, "{body}");

    // The platform filter narrows to that keyspace.
    let (_, body) = get(&pool, "/v1/search?q=ali&platform=github").await;
    let handles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["handle"].as_str().unwrap())
        .collect();
    assert_eq!(handles, ["alicorn"]);

    // Search folds the way normalization does: case and a leading @ vanish.
    let (_, body) = get(&pool, "/v1/search?q=%40ALI").await;
    assert_eq!(body["hits"].as_array().unwrap().len(), 4);

    // A retired handle stops matching. Its fuzzy neighbors still do — that
    // is what the trigram branch is for — but the retired name itself is
    // gone from the results.
    apply(&pool, 6, retire(x, "alice_1", addr(2))).await;
    let (_, body) = get(&pool, "/v1/search?q=alice_1").await;
    let handles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["handle"].as_str().unwrap())
        .collect();
    assert!(!handles.contains(&"alice_1"), "{handles:?}");
    assert!(!handles.is_empty(), "fuzzy neighbors should still match");

    // LIKE metacharacters match themselves, not everything.
    let (_, body) = get(&pool, "/v1/search?q=%25").await;
    assert_eq!(body["hits"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn replay_converges_instead_of_duplicating() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    let alice = addr(0xA1);
    let event = bind(alice, x, "111", "alice_1", 1000, true);
    apply(&pool, 1, event.clone()).await;
    apply(&pool, 1, event).await;

    let journal: i64 = sqlx::query_scalar("SELECT count(*) FROM names.events")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(journal, 1);
    let (status, body) = get(&pool, "/v1/resolve/handle/x/alice_1").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The journal conflict gates every projection write, so the one counter
    // that is not naturally convergent cannot drift on a replay either.
    apply(&pool, 2, NamesEvent::PlatformConfigured { platform_id: x }).await;
    apply(&pool, 2, NamesEvent::PlatformConfigured { platform_id: x }).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT configured_count FROM names.platforms
         WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(CHAIN)
    .bind(x.as_slice())
    .fetch_one(&pool)
    .await
    .expect("platform row");
    assert_eq!(
        count, 1,
        "replayed PlatformConfigured must not double-count"
    );
}

#[tokio::test]
async fn api_refuses_to_answer_before_the_first_window() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // No window ever committed: resolution must say "not synced", never an
    // authoritative-looking "not bound".
    let (status, body) = get(&pool, "/v1/resolve/handle/x/alice_1").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, _) = get(&pool, "/v1/search?q=ali").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // Status still answers — it is how a caller learns the sync state.
    let (status, body) = get(&pool, "/v1/status").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lastIndexedBlock"], serde_json::Value::Null);
}

#[tokio::test]
async fn nul_bytes_neither_stall_the_indexer_nor_crash_the_api() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    // An adversarial platform emits a userId with a NUL byte. The window must
    // apply — lossily, loudly — rather than stall the chain forever behind a
    // value Postgres cannot store.
    apply(
        &pool,
        1,
        bind(addr(9), x, "evil\0id", "evilhandle", 1000, false),
    )
    .await;
    let stored: String = sqlx::query_scalar(
        "SELECT user_id FROM names.ids WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(CHAIN)
    .bind(x.as_slice())
    .fetch_one(&pool)
    .await
    .expect("row applied despite the NUL");
    assert_eq!(stored, "evil\u{fffd}id");

    // And a NUL in a query is a 400, not a 500.
    let (status, _) = get(&pool, "/v1/resolve/id/x/evil%00id").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&pool, "/v1/search?q=evil%00").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn platform_and_verifier_events_land_in_ops_tables() {
    let Some((pool, _guard)) = test_pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = nodes::platform_id_for_key("x").unwrap();
    apply(&pool, 1, NamesEvent::PlatformConfigured { platform_id: x }).await;
    apply(
        &pool,
        2,
        NamesEvent::VerifierConfigured {
            platform_id: x,
            version: 1,
            verifier: addr(0xEE),
            max_future_observation: 300,
        },
    )
    .await;
    apply(
        &pool,
        3,
        NamesEvent::LatestVersionChanged {
            platform_id: x,
            version: 1,
        },
    )
    .await;
    apply(
        &pool,
        4,
        NamesEvent::VerifierRetired {
            platform_id: x,
            version: 1,
        },
    )
    .await;

    let (key, latest): (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT platform_key, latest_version FROM names.platforms
         WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(CHAIN)
    .bind(x.as_slice())
    .fetch_one(&pool)
    .await
    .expect("platform row");
    assert_eq!(key.as_deref(), Some("x"));
    assert_eq!(latest, Some(1));

    let retired: bool = sqlx::query_scalar(
        "SELECT retired FROM names.verifiers
         WHERE chain_id = $1 AND platform_id = $2 AND version = 1",
    )
    .bind(CHAIN)
    .bind(x.as_slice())
    .fetch_one(&pool)
    .await
    .expect("verifier row");
    assert!(retired);
}
