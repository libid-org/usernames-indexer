//! The CCIP-Read route, driven the way a wallet drives it.
//!
//! The unit tests in `usernames-core::ens` prove the parse and the encoding.
//! What they cannot reach is the decision: which questions get an address,
//! which get a signed null, and which get no signature at all. That logic is
//! the whole gateway, and every branch of it is here.
//!
//! Skips silently when `DATABASE_URL` is unset, like the other suites.

use alloy::{
    primitives::{
        Address,
        B256,
    },
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
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes,
};

/// This suite's own chain, so it does not collide with the others.
const CHAIN: i64 = 31341;
const RESOLVER: Address = Address::new([0xaa; 20]);
const SIGNER_KEY: &str =
    "0x00000000000000000000000000000000000000000000000000000000000a11ce";

static DB_LOCK: Mutex<()> = Mutex::const_new(());

fn platform_x() -> B256 {
    nodes::Platform::from_key("x")
        .expect("x is a known platform")
        .id()
}

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
        store.set_chain_target(1).await;
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

/// Bind a handle and commit it, so the sync gate opens and the row exists.
async fn bind(store: &ChainStore, handle: &str, owner: Address) {
    let platform = platform_x();
    let event = NamesEvent::IdentityBound {
        owner,
        id_node: nodes::id_node(platform, "42"),
        handle_node: nodes::handle_node(
            platform,
            &nodes::NormalizedHandle::from_chain(handle),
        ),
        platform_id: platform,
        user_id: "42".into(),
        handle: handle.into(),
        observed_at: 1_700_000_000,
        version: 1,
        published: true,
    };
    let mut window = store.begin_window().await.expect("begin");
    window
        .apply(
            &event,
            &LogPosition {
                block_number: 1,
                log_index: 0,
                tx_hash: B256::from([1u8; 32]),
            },
        )
        .await
        .expect("apply");
    window.commit(1).await.expect("commit");
    // The indexer records both every cycle, and the gateway refuses to answer
    // from a mirror that cannot say how far behind it is. Staleness is
    // measured against the TARGET — the block the cursor chases — so that is
    // the one a fixture must record.
    store.set_chain_head(1).await;
    store.set_chain_target(1).await;
}

/// `alice.x.handles.link`, and the same with a chain label.
fn wire_name(labels: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for label in labels.iter().chain(["handles", "link"].iter()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// Takes the labels so the node is the namehash of the name it travels with —
/// the gateway refuses a request whose two halves describe different names, and
/// so would every real client, which never builds them apart.
fn addr_call(labels: &[&str], coin: u64) -> Vec<u8> {
    let full: Vec<String> = labels
        .iter()
        .map(|l| l.to_string())
        .chain(["handles".to_string(), "link".to_string()])
        .collect();
    let mut inner = vec![0xf1, 0xcb, 0x7e, 0x06];
    inner.extend_from_slice(usernames_core::ens::namehash(&full).as_slice());
    inner.extend_from_slice(&[0u8; 24]);
    inner.extend_from_slice(&coin.to_be_bytes());
    inner
}

/// `addr(bytes32)` — the pre-ENSIP-11 shape, and what a wallet asking simply
/// "what is this name's address" still sends. It carries no coin type; the
/// gateway must read it as coin type 60, mainnet ETH.
fn legacy_addr_call() -> Vec<u8> {
    let mut inner = vec![0x3b, 0x3b, 0x57, 0xde];
    inner.extend_from_slice(&[0u8; 31]);
    inner.push(1);
    inner
}

fn resolve_call(name: &[u8], inner: &[u8]) -> Vec<u8> {
    let mut out = ens::RESOLVE_SELECTOR.to_vec();
    let mut push = |n: u64| {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&w);
    };
    push(0x40);
    push(0x40 + 32 + name.len().div_ceil(32) as u64 * 32);
    for payload in [name, inner] {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&(payload.len() as u64).to_be_bytes());
        out.extend_from_slice(&w);
        out.extend_from_slice(payload);
        out.extend(std::iter::repeat_n(
            0u8,
            payload.len().div_ceil(32) * 32 - payload.len(),
        ));
    }
    out
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

/// Pull `result` back out of the gateway's blob and check the signature is the
/// one the resolver would accept. Returns the address, or `None` for a null.
fn verify(body: &str, call: &[u8]) -> Option<Address> {
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

    // ENSIP-11 `addr` returns `bytes`: empty is the null answer.
    (result.len() >= 96).then(|| Address::from_slice(&result[64..84]))
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
    store.set_chain_target(10_000).await;

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

    let call = resolve_call(&wire_name(&["alice", "x"]), &legacy_addr_call());
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

    // And the gateway answers under its prefix.
    let call = resolve_call(
        &wire_name(&["alice", "x"]),
        &addr_call(&["alice", "x"], 0x8000_0000 | CHAIN as u64),
    );
    let uri = format!(
        "{}/{RESOLVER:?}/0x{}.json",
        usernames_api::ens::ROUTE_PREFIX,
        hex::encode(&call)
    );
    assert_eq!(get(uri).await.status(), StatusCode::OK);
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
