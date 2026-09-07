//! The CCIP-Read route, driven the way a wallet drives it.
//!
//! The unit tests in `usernames-core::ens` prove the parse and the encoding.
//! What they cannot reach is the decision: which questions get an address,
//! which get a signed null, and which get no signature at all. That logic is
//! the whole gateway, and every branch of it is here.
//!
//! Skips silently when `DATABASE_URL` is unset, like the other suites.

use alloy::{
    primitives::Address,
    signers::local::PrivateKeySigner,
};
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
    Router,
};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tokio::sync::{
    Mutex,
    MutexGuard,
};
use tower::ServiceExt;
use usernames_api::ens::{
    ChainGateway,
    Config,
    GatewayState,
    HandleSource,
};
use usernames_core::{
    db::{
        self,
        ChainStore,
    },
    ens,
};

mod common;
use common::*;

/// This suite's own chain, so it does not collide with the others.
const CHAIN: i64 = 31341;
const RESOLVER: Address = Address::new([0xaa; 20]);
const SIGNER_KEY: &str =
    "0x00000000000000000000000000000000000000000000000000000000000a11ce";

static DB_LOCK: Mutex<()> = Mutex::const_new(());

async fn gateway(
    chain_label: Option<&str>,
    max_lag_blocks: u64,
) -> Option<(Router, ChainStore, MutexGuard<'static, ()>)> {
    gateway_over(&[(CHAIN, chain_label)], max_lag_blocks).await
}

/// A gateway serving several chains from the one shared database, which is
/// what the read model's `chain_id` keying already allows.
async fn gateway_parts(
    chains: &[(i64, Option<&str>)],
    max_lag_blocks: u64,
) -> Option<(Config, ChainStore, MutexGuard<'static, ()>)> {
    let guard = DB_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("DATABASE_URL is set but connecting failed");
    db::MIGRATOR.run(&pool).await.expect("migrations");
    // Every chain this gateway will serve, not just the first: a leftover
    // cursor or head on a co-tenant chain outlives the test that set it and
    // contaminates the next one.
    for (id, _) in chains {
        for table in db::PROJECTION_TABLES {
            sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
                .bind(id)
                .execute(&pool)
                .await
                .expect("cleanup");
        }
        sqlx::query("DELETE FROM names.chain_metadata WHERE chain_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("metadata cleanup");
    }

    // Every chain starts SYNCED AND EMPTY, which is a different state from
    // never indexed: an empty window is committed so a cursor exists, and the
    // head is recorded. A mirror with neither cannot say how far behind it is,
    // and the gateway refuses rather than signing a null — correct in
    // production, and previously invisible here because "unknown" was read as
    // "fresh".
    for (id, _) in chains {
        let store = ChainStore::new(pool.clone(), *id);
        store
            .begin_window()
            .await
            .expect("begin")
            .commit(1)
            .await
            .expect("commit");
        store.set_chain_head(1).await;
        // The gate measures the cursor against the TARGET — what the indexer
        // intends to reach — not the raw head, so a fixture that records only
        // the head reads as "cannot tell" and refuses every query.
        store.set_chain_target(1, 120).await;
    }

    let store = ChainStore::new(pool.clone(), chains[0].0);
    let config = Config {
        resolver: RESOLVER,
        chains: chains
            .iter()
            .map(|(id, label)| {
                (
                    *id as u64,
                    ChainGateway {
                        label: label.map(str::to_string),
                        source: HandleSource::Mirror(ChainStore::new(pool.clone(), *id)),
                    },
                )
            })
            .collect(),
        ttl_secs: 300,
        max_lag_blocks,
        signer: std::sync::Arc::new(SIGNER_KEY.parse::<PrivateKeySigner>().expect("key")),
    };
    Some((config, store, guard))
}

/// The gateway route alone, which is what most of these tests exercise.
async fn gateway_over(
    chains: &[(i64, Option<&str>)],
    max_lag_blocks: u64,
) -> Option<(Router, ChainStore, MutexGuard<'static, ()>)> {
    let (config, store, guard) = gateway_parts(chains, max_lag_blocks).await?;
    Some((
        usernames_api::ens::router(GatewayState::new(config)),
        store,
        guard,
    ))
}

async fn ask(router: &Router, sender: Address, call: &[u8]) -> (StatusCode, String) {
    let uri = format!("/{sender}/0x{}.json", hex::encode(call));
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router");
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();
    (status, text)
}

/// The signed result inside a gateway answer, once the signature is checked
/// to be one the resolver would accept. These are the bytes the callback
/// returns to the wallet, in whatever shape the record call asked for.
fn signed_result(body: &str, call: &[u8]) -> Vec<u8> {
    let value: serde_json::Value = serde_json::from_str(body).expect("json");
    let data = hex::decode(
        value["data"]
            .as_str()
            .expect("a data field")
            .trim_start_matches("0x"),
    )
    .expect("hex");

    // (bytes result, uint64 expires, bytes signature), by hand.
    let word = |at: usize| -> u64 {
        let mut n = [0u8; 8];
        n.copy_from_slice(&data[at + 24..at + 32]);
        u64::from_be_bytes(n)
    };
    let result_at = word(0) as usize;
    let expires = word(32);
    let sig_at = word(64) as usize;
    let result_len = word(result_at) as usize;
    let result = &data[result_at + 32..result_at + 32 + result_len];
    let sig_len = word(sig_at) as usize;
    let signature = &data[sig_at + 32..sig_at + 32 + sig_len];

    let digest = ens::signature_digest(RESOLVER, expires, call, result);
    let recovered = alloy::primitives::Signature::try_from(signature)
        .expect("65-byte signature")
        .recover_address_from_prehash(&digest)
        .expect("recover");
    assert_eq!(
        recovered,
        SIGNER_KEY.parse::<PrivateKeySigner>().unwrap().address(),
        "the resolver would recover a signer it does not trust"
    );
    result.to_vec()
}

/// The address in a gateway answer, or `None` for a null in either shape.
fn verify(body: &str, call: &[u8]) -> Option<Address> {
    let result = signed_result(body, call);
    // Both shapes, because the caller chooses which to ask in and a helper that
    // knows only one reports a correct legacy answer as a null.
    match result.len() {
        // `addr(bytes32)` returns a bare `address`; the zero address is null.
        32 => {
            let who = Address::from_slice(&result[12..32]);
            (!who.is_zero()).then_some(who)
        }
        // ENSIP-11's `addr(bytes32,uint256)` returns `bytes`: offset, length,
        // then the payload. An empty byte string is null.
        n if n >= 96 => Some(Address::from_slice(&result[64..84])),
        _ => None,
    }
}

macro_rules! gateway_or_skip {
    ($label:expr, $lag:expr) => {
        match gateway($label, $lag).await {
            Some(parts) => parts,
            None => return,
        }
    };
}

#[tokio::test]
async fn a_bound_handle_resolves_and_the_signature_verifies() {
    let (router, store, _g) = gateway_or_skip!(None, 32);
    let owner = Address::from([0xbe; 20]);
    bind(&store, "alice", owner).await;

    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(verify(&body, &call), Some(owner));
}

#[tokio::test]
async fn a_name_nobody_holds_gets_a_signed_null() {
    // The point of signing a null: a wallet must be able to trust "nobody
    // holds this" as much as it trusts an address.
    let (router, _store, _g) = gateway_or_skip!(None, 32);
    let call = resolve_call(
        &wire_name(&["nobody", "x"]),
        &addr_call(&["nobody", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(verify(&body, &call), None);
}

/// A chain this gateway does not serve must be REFUSED, not answered null.
///
/// The resolver holds one `urls` list for every query it can ever answer — it
/// cannot route by coin type — and ERC-3668 has the client walk that list
/// until something succeeds. A signed null is a success, so answering here
/// would end the walk and deny a binding that the next endpoint in the list
/// exists to serve. This is the whole reason the gateway knows a SET of
/// chains rather than one.
#[tokio::test]
async fn a_chain_this_gateway_does_not_serve_is_refused_not_signed() {
    let (router, store, _g) = gateway_or_skip!(None, 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;

    // Base, while this gateway serves only the test chain.
    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_2105),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(!body.contains("\"data\""), "a refusal must carry no answer");
}

/// A coin type naming no EVM chain at all IS a signed null: no libID binding
/// is ever a Bitcoin address, and knowing that needs no chain.
#[tokio::test]
async fn a_non_evm_coin_type_is_answered_null() {
    let (router, store, _g) = gateway_or_skip!(None, 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;

    let call = resolve_call(&wire_name(&["alice", "x"]), &addr_call(&["alice", "x"], 0));
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(verify(&body, &call), None);
}

#[tokio::test]
async fn a_chain_label_narrows_and_never_widens() {
    let (router, store, _g) = gateway_or_skip!(Some("eden"), 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;
    let coin = 0x8000_0000 | CHAIN as u64;

    // Our label: answered.
    let ours = resolve_call(
        &wire_name(&["alice", "x", "eden"]),
        &addr_call(&["alice", "x", "eden"], coin),
    );
    let (_, body) = ask(&router, RESOLVER, &ours).await;
    assert!(verify(&body, &ours).is_some());

    // Another chain's label, same coin type: not this gateway's name.
    let theirs = resolve_call(
        &wire_name(&["alice", "x", "base"]),
        &addr_call(&["alice", "x", "base"], coin),
    );
    let (_, body) = ask(&router, RESOLVER, &theirs).await;
    assert_eq!(verify(&body, &theirs), None);
}

#[tokio::test]
async fn a_record_that_is_not_addr_degrades_to_null() {
    // A client asking for `text()` must not see an error on a name that
    // resolves fine for addresses.
    let (router, store, _g) = gateway_or_skip!(None, 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;

    let call = resolve_call(&wire_name(&["alice", "x"]), &[0xaa, 0xbb, 0xcc, 0xdd]);
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(verify(&body, &call), None);
}

#[tokio::test]
async fn a_name_this_build_cannot_read_is_null_rather_than_an_error() {
    let (router, _store, _g) = gateway_or_skip!(None, 32);
    let coin = 0x8000_0000 | CHAIN as u64;
    for labels in [
        ["alice", "myspace"].as_slice(), // a platform nobody knows
        ["alice", "x", "toolong"].as_slice(), // a chain label that is not ours
    ] {
        let call = resolve_call(&wire_name(labels), &addr_call(labels, coin));
        let (status, body) = ask(&router, RESOLVER, &call).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(verify(&body, &call), None);
    }
}

/// The null for an unreadable name takes the shape of the record asked in.
///
/// `addr(bytes32)` returns an `address`, so its null is 32 zero bytes; the
/// ENSIP-11 form returns `bytes`, whose null is empty. A null built before the
/// record was read came out in the second shape whatever the caller asked, and
/// a wallet decoding the first read the `bytes` offset word as the address
/// 0x…0020 — signed, so authoritative.
#[tokio::test]
async fn an_unreadable_name_is_null_in_the_shape_the_caller_asked_in() {
    // Mainnet, because the legacy form is coin type 60 and a gateway that does
    // not serve it refuses unsigned rather than answering.
    let Some((router, _store, _g)) = gateway_over(&[(1, None)], 32).await else {
        return;
    };
    let labels = ["alice", "myspace"];
    let call = resolve_call(&wire_name(&labels), &legacy_addr_call(&labels));
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        signed_result(&body, &call),
        [0u8; 32],
        "the legacy null is the zero address, 32 bytes"
    );
    assert_eq!(verify(&body, &call), None);
}

/// A chain this gateway does not serve keeps its unsigned refusal for an
/// unreadable name too: the null is never signed ahead of the chain check, so
/// the client walks on to a sibling that may read the name.
#[tokio::test]
async fn an_unreadable_name_off_our_chains_is_still_refused_unsigned() {
    let (router, _store, _g) = gateway_or_skip!(None, 32);
    let labels = ["alice", "myspace"];
    let call = resolve_call(&wire_name(&labels), &addr_call(&labels, 0x8000_2105));
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(!body.contains("\"data\""), "a refusal must carry no answer");
}

#[tokio::test]
async fn a_request_for_another_resolver_is_refused_not_signed() {
    // The signature names its target, so signing for a caller-supplied sender
    // would lend this key's authority to any contract that asked.
    let (router, _store, _g) = gateway_or_skip!(None, 32);
    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let (status, _) = ask(&router, Address::from([0xcc; 20]), &call).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_stale_mirror_refuses_rather_than_signing_a_null() {
    // The distinction the lag gate exists for: a signed null asserts that no
    // binding exists, and a mirror this far behind has not earned that.
    let (router, store, _g) = gateway_or_skip!(None, 0);
    bind(&store, "alice", Address::from([0xbe; 20])).await;
    // Far ahead of the cursor, as the TARGET: that is the quantity the gate
    // compares, and the head alone no longer moves it.
    store.set_chain_target(10_000, 120).await;

    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(!body.contains("\"data\""), "a refusal must carry no answer");
}

#[tokio::test]
async fn garbage_in_the_path_is_the_callers_error() {
    let (router, _store, _g) = gateway_or_skip!(None, 32);
    for call in [vec![0xde, 0xad, 0xbe, 0xef], vec![]] {
        let (status, _) = ask(&router, RESOLVER, &call).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

/// The fix, stated as a test: one gateway, two chains, and each coin type gets
/// its own chain's answer rather than the first one's.
#[tokio::test]
async fn one_gateway_answers_for_every_chain_it_serves() {
    const OTHER: i64 = 31342;
    let Some((router, store, _g)) =
        gateway_over(&[(CHAIN, None), (OTHER, None)], 32).await
    else {
        return;
    };
    let here = Address::from([0xbe; 20]);
    let there = Address::from([0xed; 20]);
    bind(&store, "alice", here).await;

    // The same handle, bound to a different wallet on the other chain.
    let other_store = ChainStore::new(
        sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap(),
        OTHER,
    );
    bind(&other_store, "alice", there).await;

    for (chain, expected) in [(CHAIN, here), (OTHER, there)] {
        let call = resolve_call(
            &wire_name(&["alice", "x"]),
            &addr_call(&["alice", "x"], 0x8000_0000 | chain as u64),
        );
        let (status, body) = ask(&router, RESOLVER, &call).await;
        assert_eq!(status, StatusCode::OK, "chain {chain}: {body}");
        assert_eq!(
            verify(&body, &call),
            Some(expected),
            "chain {chain} answered with another chain's wallet"
        );
    }
}

/// A mirror that cannot report its own position must refuse, not deny.
///
/// This is the fail-open the lag gate closes. `chain_head` and `cursor` are
/// both absent on a fresh database, and again while `prepare` replays a chain
/// after a version bump — and an unreadable Postgres looks the same. Folding
/// that into "no lag" made the gateway sign an authoritative "nobody holds
/// this" for every name, cached by wallets for the whole `ENS_TTL_SECS`
/// window.
#[tokio::test]
async fn a_mirror_that_cannot_report_its_position_refuses() {
    let (router, store, _g) = gateway_or_skip!(None, 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;

    // Wipe what the mirror knows about its own progress, leaving the binding
    // in place: the rows say one thing, the cursor says nothing.
    sqlx::query("DELETE FROM names.chain_metadata WHERE chain_id = $1")
        .bind(CHAIN)
        .execute(store.pool())
        .await
        .expect("clear metadata");

    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(!body.contains("\"data\""), "a refusal must carry no answer");
}

/// The block gate cannot see a stopped indexer: target and cursor are both
/// its own writes, so they freeze together and lag reads zero for as long as
/// it stays down — signed nulls for bindings made since, and old addresses
/// after a rebind, for the whole `ENS_TTL_SECS` each. The report the indexer
/// makes beside the target expires on the indexer's own schedule, and that is
/// what notices.
#[tokio::test]
async fn an_expired_report_is_too_stale_however_small_the_lag() {
    let (router, store, _g) = gateway_or_skip!(None, 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;
    // Caught up by the block measure, and the report long expired.
    store.set_chain_target_valid_until(1).await;

    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("expired"), "{body}");
    assert!(!body.contains("\"data\""), "a refusal must carry no answer");
}

/// One chain's dead indexer is that chain's problem. Every row the gate reads
/// is keyed by chain, so the refusal reaches the chain whose report expired
/// and no other — the API keeps answering for everyone else. And supervision
/// sees exactly what the gate sees, one row per chain.
#[tokio::test]
async fn a_dead_indexer_on_one_chain_does_not_touch_another() {
    const OTHER: i64 = 31342;
    let Some((router, store, _g)) =
        gateway_over(&[(CHAIN, None), (OTHER, None)], 32).await
    else {
        return;
    };
    let here = Address::from([0xbe; 20]);
    bind(&store, "alice", here).await;
    let other_store = ChainStore::new(store.pool().clone(), OTHER);
    bind(&other_store, "alice", Address::from([0xed; 20])).await;
    // The other chain's indexer stopped long ago.
    other_store.set_chain_target_valid_until(1).await;

    let dead = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | OTHER as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &dead).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let alive = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &alive).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(verify(&body, &alive), Some(here));

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let status: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let stale_of = |id: i64| {
        status["chains"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["chainId"] == id)
            .map(|c| c["stale"].as_bool().unwrap())
    };
    assert_eq!(stale_of(OTHER), Some(true), "{status}");
    assert_eq!(stale_of(CHAIN), Some(false), "{status}");
}

/// The eden testnet resolves, and that it does is the whole point of matching
/// coin types forwards instead of decoding them backwards.
///
/// Eden's chain id is 3735928814 — `0xDEADBFEE`, bit 31 already set — so
/// `0x80000000 | chainId` returns it unchanged. A gateway that decoded the
/// coin type would get 1588445166 and refuse every eden name. Comparing
/// against the chains it serves gets it right, and the only thing that is
/// genuinely ambiguous — serving both colliding chains at once — is refused at
/// startup instead.
#[tokio::test]
async fn the_eden_testnet_resolves_despite_its_chain_id() {
    const EDEN: i64 = 3_735_928_814;
    let Some((router, store, _g)) = gateway_over(&[(EDEN, Some("eden"))], 32).await
    else {
        return;
    };
    let owner = Address::from([0xed; 20]);
    bind(&store, "alice", owner).await;

    // Exactly what a wallet on eden sends: `0x80000000 | 3735928814`, which is
    // 3735928814 itself.
    let call = resolve_call(
        &wire_name(&["alice", "x", "eden"]),
        &addr_call(&["alice", "x", "eden"], 0x8000_0000u64 | EDEN as u64),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "eden must resolve: {body}");
    assert_eq!(verify(&body, &call), Some(owner));
}

/// The legacy shape decodes, and a gateway that does not serve mainnet refuses
/// it UNSIGNED.
///
/// Both halves matter. The decode had no coverage at all, and the refusal is
/// the multi-chain invariant: coin type 60 names Ethereum mainnet, so a
/// gateway serving only some other chain has nothing to say about it. Saying
/// so with a 5xx keeps the client walking the resolver's `urls`; a signed null
/// would be an authoritative "nobody holds this" and would end that walk at
/// the first gateway asked.
#[tokio::test]
async fn the_legacy_addr_shape_is_refused_unsigned_off_mainnet() {
    let (router, store, _g) = gateway_or_skip!(None, 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;

    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &legacy_addr_call(&["alice", "x"]),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a signed null here would end the client's url walk: {body}"
    );
}

/// What the binary actually serves: both halves on one router, with the
/// middleware over the whole of it.
///
/// The halves were only ever built apart, so three things had no coverage —
/// that the gateway route does not swallow the API's namespace, that a `/v1`
/// path which matches nothing is a 404 rather than the gateway's "sender is
/// not an address", and that CORS is applied once rather than layered twice.
#[tokio::test]
async fn the_merged_router_keeps_both_halves_intact() {
    let Some((config, store, _g)) = gateway_parts(&[(CHAIN, None)], 32).await else {
        return;
    };
    let owner = Address::from([0xbe; 20]);
    bind(&store, "alice", owner).await;

    let app = usernames_api::build_router(
        usernames_core::api::AppState::new(store.clone(), Address::ZERO),
        Some(config),
    );

    let get = |uri: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .uri(uri)
                    .header("origin", "https://handle.link")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router")
        }
    };

    // The API half still answers, and carries exactly ONE set of CORS headers.
    let response = get("/v1/status".into()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get_all("vary").iter().count(),
        1,
        "two CORS layers would each append a Vary"
    );

    // A `/v1` path that matches nothing is the API's 404 — not the gateway
    // claiming every two-segment path and reporting a malformed sender.
    assert_eq!(
        get("/v1/typo".into()).await.status(),
        StatusCode::NOT_FOUND,
        "the gateway route must not claim the API's namespace"
    );

    // And the gateway answers under its prefix — with the CORS header, on the
    // answer and on a refusal alike. This is the regression `build_router`
    // exists to prevent: a layer applied inside `api::router` leaves the
    // gateway route without `Access-Control-Allow-Origin`, and a browser
    // running the batch gateway in the page never sees the answer.
    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let uri = format!(
        "{}/{RESOLVER:?}/0x{}.json",
        usernames_api::ens::ROUTE_PREFIX,
        hex::encode(&call)
    );
    let allowed_origin = |response: &axum::response::Response| {
        response
            .headers()
            .get("access-control-allow-origin")
            .map(|v| v.as_bytes().to_vec())
    };
    let answer = get(uri).await;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(
        allowed_origin(&answer).as_deref(),
        Some(&b"*"[..]),
        "the gateway route carries no CORS header"
    );
    let refused = get(format!(
        "{}/not-an-address/0x00.json",
        usernames_api::ens::ROUTE_PREFIX
    ))
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        allowed_origin(&refused).as_deref(),
        Some(&b"*"[..]),
        "a refusal without the CORS header is one a browser cannot even read"
    );
}

/// The request names one thing twice — once as a name, once as the node inside
/// the record call — and a caller that lets the two disagree is refused rather
/// than answered. The signature would cover both halves, so an answer here
/// would be true of the name and attributed to the node.
#[tokio::test]
async fn a_node_that_is_not_this_name_is_refused() {
    let (router, store, _g) = gateway_or_skip!(None, 32);
    bind(&store, "alice", Address::from([0xbe; 20])).await;

    let coin = 0x8000_0000 | CHAIN as u64;
    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        // The namehash of a different name entirely.
        &addr_call(&["bob", "x"], coin),
    );
    let (status, body) = ask(&router, RESOLVER, &call).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And the well-formed request it differs from still answers, so the
    // refusal is about the disagreement rather than about the name.
    let ok = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], coin),
    );
    let (status, body) = ask(&router, RESOLVER, &ok).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(verify(&body, &ok).is_some());
}

/// A name outside this resolver's domain is refused, not answered.
///
/// Every answer here is signed, and a signed null is an authoritative "nobody
/// holds this". This gateway has no standing to say that about `vitalik.eth`.
/// The check has to run before the record is looked at, because a record it
/// cannot answer would otherwise short-circuit to a null for any name at all —
/// and before label hygiene, because `Vitalik.eth` is foreign first and
/// unnormalized second, and "unreadable" would have earned it a signed null.
#[tokio::test]
async fn a_name_outside_the_domain_is_refused_whatever_the_record() {
    let (router, _store, _g) = gateway_or_skip!(None, 32);
    let node = usernames_core::ens::namehash(&["vitalik".to_string(), "eth".to_string()]);

    let mut addr = vec![0xf1, 0xcb, 0x7e, 0x06];
    addr.extend_from_slice(node.as_slice());
    addr.extend_from_slice(&[0u8; 24]);
    addr.extend_from_slice(&(0x8000_0000u64 | CHAIN as u64).to_be_bytes());

    for inner in [addr, vec![0xaa, 0xbb, 0xcc, 0xdd]] {
        for name in [
            &b"\x07vitalik\x03eth\x00"[..],
            &b"\x07Vitalik\x03eth\x00"[..],
        ] {
            let call = resolve_call(name, &inner);
            let (status, body) = ask(&router, RESOLVER, &call).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
    }
}
