//! The CCIP-Read route, driven the way a wallet drives it.
//!
//! The unit tests in `usernames-core::ens` prove the parse and the encoding.
//! What they cannot reach is the decision: which questions get an address,
//! which get a signed null, and which get no signature at all. That logic is
//! the whole gateway, and every branch of it is here.
//!
//! Skips silently when `DATABASE_URL` is unset, like the other suites.

use std::{
    fmt,
    sync::Arc,
    time::Duration,
};

use alloy::{
    primitives::{
        Address,
        Bytes,
        Signature,
    },
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
    response::Response,
    Router,
};
use http_body_util::BodyExt;
use libid_signer::ManagedSigner;
use sqlx::PgPool;
use tokio::sync::{
    Mutex,
    MutexGuard,
};
use tower::ServiceExt;
use usernames_api::ens::{
    Config,
    GatewayRefusal,
    GatewayResponse,
    GatewayState,
    GatewayStatus,
    ServedChain,
    ROUTE_PREFIX,
};
use usernames_core::{
    api::AppState,
    db::{
        self,
        ChainStore,
        Store,
    },
    ens,
};

use usernames_fixtures::{
    database_url,
    resolve_call,
    Labels,
    LockWaits,
    Seed,
};

/// This suite's own chain, so it does not collide with the others.
const CHAIN: i64 = 31341;
/// A second chain in the same store, for the multi-chain cases.
const OTHER: i64 = 31342;
/// The eden testnet: chain id `0xDEADBFEE`, bit 31 already set, so its
/// ENSIP-11 coin type is the id itself.
const EDEN: i64 = 3_735_928_814;
/// The chain id that lands on eden's coin type from the other side of the OR.
const TWIN: i64 = 1_588_445_166;
/// Every chain any test here may write, so a fixture can clear them all: the
/// gateway serves whatever the store holds, so a leftover would be served.
const SUITE_CHAINS: [i64; 5] = [CHAIN, OTHER, EDEN, TWIN, 1];
/// The suite's resolver, which sits in the mainnet registry.
const RESOLVER: Address = Address::new([0xaa; 20]);
const SIGNER_KEY: &str =
    "0x00000000000000000000000000000000000000000000000000000000000a11ce";

static DB_LOCK: Mutex<()> = Mutex::const_new(());

/// The gateway over chains of the database the suites share: every chain
/// the suite writes emptied, the served ones synced and empty, held under
/// the suite's lock until [`Gateway::done`].
struct Gateway {
    /// What the gateway serves with: its store, domain, resolver and signer.
    config: Config,
    /// The first chain served.
    store: ChainStore,
    /// The database the suites share.
    pool: PgPool,
    lock: MutexGuard<'static, ()>,
}

impl Gateway {
    /// A gateway serving `chains`, refusing an index more than
    /// `max_lag_blocks` behind; `None` when `DATABASE_URL` is unset.
    async fn open(chains: &[i64], max_lag_blocks: u64) -> Option<Self> {
        let lock = DB_LOCK.lock().await;
        let url = database_url()?;
        let pool = PgPool::connect(&url)
            .await
            .expect("DATABASE_URL is set but connecting failed");
        db::MIGRATOR.run(&pool).await.expect("migrations");
        // Every chain this suite may have written, not just the ones this test
        // serves: the store IS what the gateway serves, so a leftover chain from
        // another test would be served here too.
        for id in SUITE_CHAINS {
            ChainStore::new(pool.clone(), id).forget().await;
        }

        // Every chain starts SYNCED AND EMPTY, which is a different state from
        // never indexed: an empty window is committed so a cursor exists, and the
        // head is recorded. An index with neither cannot say how far behind it is,
        // and the gateway refuses rather than signing a null.
        for id in chains {
            let writer = ChainStore::new(pool.clone(), *id).writer().await;
            writer
                .begin_window()
                .await
                .expect("begin")
                .commit(1)
                .await
                .expect("commit");
            writer.set_chain_head(1).await;
            // The gate measures the cursor against the TARGET — what the indexer
            // intends to reach — not the raw head, so a fixture that records only
            // the head reads as "cannot tell" and refuses every query.
            writer.set_chain_target(1, Duration::from_secs(120)).await;
        }

        let store = ChainStore::new(pool.clone(), chains[0]);
        let config = Config {
            domain: ens::DEFAULT_DOMAIN.parse().expect("domain"),
            resolver: RESOLVER,
            ens_chain: 1,
            store: db::Store::new(pool.clone()),
            ttl: Duration::from_secs(300),
            max_lag_blocks,
            signer: Arc::new(ManagedSigner::Local(
                SIGNER_KEY.parse::<PrivateKeySigner>().expect("key"),
            )),
        };
        Some(Self {
            config,
            store,
            pool,
            lock,
        })
    }

    /// The gateway route alone, over [`Gateway::config`].
    fn router(&self) -> Router {
        GatewayState::new(self.config.clone()).router()
    }

    /// The test is done with the database; the suite's next test may take it.
    fn done(self) {
        let Self { lock, .. } = self;
        drop(lock);
    }
}

/// What a chain's indexer declares at startup.
trait DeclareNames {
    /// What a chain's indexer does at startup: declare the names the chain goes
    /// by. The gateway learns them from the store and nowhere else.
    async fn declare_names(&self, names: &[&str]);
}

impl DeclareNames for ChainStore {
    async fn declare_names(&self, names: &[&str]) {
        let names: Vec<_> = names
            .iter()
            .map(|name| name.parse::<ens::ChainName>().expect("a chain name"))
            .collect();
        let writer = self.acquire_writer().await.expect("lease");
        self.set_chain_names(&writer, &names)
            .await
            .expect("names declared");
        writer.release().await.expect("release");
    }
}

/// One forwarded call to the gateway.
trait Ask {
    async fn ask(&self, sender: Address, call: &[u8]) -> (StatusCode, Reply);
}

impl Ask for Router {
    async fn ask(&self, sender: Address, call: &[u8]) -> (StatusCode, Reply) {
        let uri = format!("/{sender}/0x{}.json", hex::encode(call));
        let response = self
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .expect("router");
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, Reply(body.into()))
    }
}

/// One chain's row of the gateway's status.
trait Row {
    /// The status row of one chain; a chain the status does not list fails the
    /// test.
    fn row(&self, chain_id: i64) -> &ServedChain;
}

impl Row for GatewayStatus {
    fn row(&self, chain_id: i64) -> &ServedChain {
        self.chains
            .iter()
            .find(|row| i64::try_from(row.chain_id) == Ok(chain_id))
            .unwrap_or_else(|| panic!("chain {chain_id} is listed: {self:?}"))
    }
}

/// What the gateway sent back: a signed answer or a refusal, read as one of
/// its two types.
struct Reply(Bytes);

impl Reply {
    /// The signed answer; anything else fails the test.
    fn answer(&self) -> GatewayResponse {
        serde_json::from_slice(&self.0)
            .unwrap_or_else(|e| panic!("not an answer ({e}): {self}"))
    }

    /// The refusal's message. A refusal that also reads as an answer fails
    /// the test: it must carry none.
    fn refused(&self) -> String {
        assert!(
            serde_json::from_slice::<GatewayResponse>(&self.0).is_err(),
            "a refusal must carry no answer: {self}"
        );
        let refusal: GatewayRefusal = serde_json::from_slice(&self.0)
            .unwrap_or_else(|e| panic!("not a refusal ({e}): {self}"));
        refusal.message
    }

    /// The signed result inside a gateway answer, once the signature is checked
    /// to be one the resolver would accept. These are the bytes the callback
    /// returns to the wallet, in whatever shape the record call asked for.
    fn signed_result(&self, call: &[u8]) -> Vec<u8> {
        let (result, expires, signature) =
            <(Bytes, u64, Bytes)>::abi_decode_params(&self.answer().data)
                .expect("(bytes result, uint64 expires, bytes signature)");

        let digest = ens::Reply {
            result: result.to_vec(),
            expires,
        }
        .digest(RESOLVER, call);
        let recovered = Signature::try_from(signature.as_ref())
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
    fn verify(&self, call: &[u8]) -> Option<Address> {
        let result = self.signed_result(call);
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
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

#[tokio::test]
async fn a_bound_handle_resolves_and_the_signature_verifies() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let owner = Address::from([0xbe; 20]);
    gateway.store.bind("alice", owner).await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&call), Some(owner));
    gateway.done();
}

/// A private binding resolves too, as `resolveHandle` does on chain.
#[tokio::test]
async fn a_private_binding_resolves() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let owner = Address::from([0xbe; 20]);
    gateway.store.bind_private("alice", owner).await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&call), Some(owner));
    gateway.done();
}

#[tokio::test]
async fn a_name_nobody_holds_gets_a_signed_null() {
    // The point of signing a null: a wallet must be able to trust "nobody
    // holds this" as much as it trusts an address.
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let call = resolve_call(
        &["nobody", "x"].wire_name(),
        &["nobody", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&call), None);
    gateway.done();
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
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;

    // Base, while this gateway serves only the test chain.
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_2105),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    body.refused();
    gateway.done();
}

/// A coin type naming no EVM chain at all IS a signed null: no libID binding
/// is ever a Bitcoin address, and knowing that needs no chain.
#[tokio::test]
async fn a_non_evm_coin_type_is_answered_null() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;

    let call = resolve_call(&["alice", "x"].wire_name(), &["alice", "x"].addr_call(0));
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&call), None);
    gateway.done();
}

#[tokio::test]
async fn a_chain_label_narrows_and_never_widens() {
    let Some(gateway) = Gateway::open(&[EDEN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;
    gateway.store.declare_names(&["eden"]).await;
    let coin = 0x8000_0000 | EDEN as u64;

    // Our label: answered.
    let ours = resolve_call(
        &["alice", "x", "eden"].wire_name(),
        &["alice", "x", "eden"].addr_call(coin),
    );
    let (_, body) = router.ask(RESOLVER, &ours).await;
    assert!(body.verify(&ours).is_some());

    // Another chain's label, same coin type: not this gateway's name.
    let theirs = resolve_call(
        &["alice", "x", "base"].wire_name(),
        &["alice", "x", "base"].addr_call(coin),
    );
    let (_, body) = router.ask(RESOLVER, &theirs).await;
    assert_eq!(body.verify(&theirs), None);
    gateway.done();
}

/// A chain may go by several names, and each one names it alike: the
/// unlabelled form and every declared label resolve to the same binding.
#[tokio::test]
async fn every_name_a_chain_declares_resolves_it() {
    let Some(gateway) = Gateway::open(&[EDEN], 32).await else {
        return;
    };
    let router = gateway.router();
    let owner = Address::from([0xbe; 20]);
    gateway.store.bind("alice", owner).await;
    gateway.store.declare_names(&["eden", "eden-testnet"]).await;
    let coin = 0x8000_0000 | EDEN as u64;

    for labels in [
        ["alice", "x"].as_slice(),
        ["alice", "x", "eden"].as_slice(),
        ["alice", "x", "eden-testnet"].as_slice(),
    ] {
        let call = resolve_call(&labels.wire_name(), &labels.addr_call(coin));
        let (status, body) = router.ask(RESOLVER, &call).await;
        assert_eq!(status, StatusCode::OK, "{labels:?}: {body}");
        assert_eq!(body.verify(&call), Some(owner), "{labels:?}");
    }
    gateway.done();
}

#[tokio::test]
async fn a_record_that_is_not_addr_degrades_to_null() {
    // A client asking for `text()` must not see an error on a name that
    // resolves fine for addresses.
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;

    let call = resolve_call(&["alice", "x"].wire_name(), &[0xaa, 0xbb, 0xcc, 0xdd]);
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&call), None);
    gateway.done();
}

#[tokio::test]
async fn a_name_this_build_cannot_read_is_null_rather_than_an_error() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let coin = 0x8000_0000 | CHAIN as u64;
    for labels in [
        ["alice", "myspace"].as_slice(), // a platform nobody knows
        ["alice", "x", "toolong"].as_slice(), // a chain label that is not ours
    ] {
        let call = resolve_call(&labels.wire_name(), &labels.addr_call(coin));
        let (status, body) = router.ask(RESOLVER, &call).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.verify(&call), None);
    }
    gateway.done();
}

/// The null for an unreadable name takes the shape of the record asked in.
///
/// `addr(bytes32)` returns an `address`, so its null is 32 zero bytes; the
/// ENSIP-11 form returns `bytes`, whose null is empty. A wallet that asked in
/// the first shape and got the second would read the `bytes` offset word as
/// the address 0x…0020, signed and so authoritative.
#[tokio::test]
async fn an_unreadable_name_is_null_in_the_shape_the_caller_asked_in() {
    // Mainnet, because the legacy form is coin type 60 and a gateway that does
    // not serve it refuses unsigned rather than answering.
    let Some(gateway) = Gateway::open(&[1], 32).await else {
        return;
    };
    let router = gateway.router();
    let labels = ["alice", "myspace"];
    let call = resolve_call(&labels.wire_name(), &labels.legacy_addr_call());
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body.signed_result(&call),
        [0u8; 32],
        "the legacy null is the zero address, 32 bytes"
    );
    assert_eq!(body.verify(&call), None);
    gateway.done();
}

/// A chain this gateway does not serve keeps its unsigned refusal for an
/// unreadable name too: the null is never signed ahead of the chain check, so
/// the client walks on to a sibling that may read the name.
#[tokio::test]
async fn an_unreadable_name_off_our_chains_is_still_refused_unsigned() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let labels = ["alice", "myspace"];
    let call = resolve_call(&labels.wire_name(), &labels.addr_call(0x8000_2105));
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    body.refused();
    gateway.done();
}

#[tokio::test]
async fn a_request_for_another_resolver_is_refused_not_signed() {
    // The signature names its target, so signing for a caller-supplied sender
    // would lend this key's authority to any contract that asked.
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, _) = router.ask(Address::from([0xcc; 20]), &call).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    gateway.done();
}

/// A deployment may answer under a subname of the domain. Its names carry
/// the subname, and the parent's own names are not its to speak about.
#[tokio::test]
async fn a_gateway_under_a_subname_answers_its_names_and_refuses_the_parents() {
    let Some(mut gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    gateway.config.domain = "testnet.handles.link".parse().expect("domain");
    let router = gateway.router();
    let owner = Address::from([0xbe; 20]);
    gateway.store.bind("alice", owner).await;
    let coin = 0x8000_0000 | CHAIN as u64;

    // `wire_name` appends `handles.link`, so the subname rides as a label.
    let ours = resolve_call(
        &["alice", "x", "testnet"].wire_name(),
        &["alice", "x", "testnet"].addr_call(coin),
    );
    let (status, body) = router.ask(RESOLVER, &ours).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&ours), Some(owner));

    let parents =
        resolve_call(&["alice", "x"].wire_name(), &["alice", "x"].addr_call(coin));
    let (status, body) = router.ask(RESOLVER, &parents).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    gateway.done();
}

#[tokio::test]
async fn a_stale_index_refuses_rather_than_signing_a_null() {
    // The distinction the lag gate exists for: a signed null asserts that no
    // binding exists, and an index this far behind has not earned that.
    let Some(gateway) = Gateway::open(&[CHAIN], 0).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;
    // Far ahead of the cursor, as the TARGET: that is the quantity the gate
    // compares; the head alone does not move it.
    gateway
        .store
        .writer()
        .await
        .set_chain_target(10_000, Duration::from_secs(120))
        .await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    body.refused();
    gateway.done();
}

#[tokio::test]
async fn garbage_in_the_path_is_the_callers_error() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    for call in [vec![0xde, 0xad, 0xbe, 0xef], vec![]] {
        let (status, _) = router.ask(RESOLVER, &call).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    gateway.done();
}

/// One gateway, two chains: each coin type gets its own chain's answer rather
/// than the first one's.
#[tokio::test]
async fn one_gateway_answers_for_every_chain_it_serves() {
    let Some(gateway) = Gateway::open(&[CHAIN, OTHER], 32).await else {
        return;
    };
    let router = gateway.router();
    let here = Address::from([0xbe; 20]);
    let there = Address::from([0xed; 20]);
    gateway.store.bind("alice", here).await;

    // The same handle, bound to a different wallet on the other chain.
    let other_store = ChainStore::new(gateway.pool.clone(), OTHER);
    other_store.bind("alice", there).await;

    for (chain, expected) in [(CHAIN, here), (OTHER, there)] {
        let call = resolve_call(
            &["alice", "x"].wire_name(),
            &["alice", "x"].addr_call(0x8000_0000 | chain as u64),
        );
        let (status, body) = router.ask(RESOLVER, &call).await;
        assert_eq!(status, StatusCode::OK, "chain {chain}: {body}");
        assert_eq!(
            body.verify(&call),
            Some(expected),
            "chain {chain} answered with another chain's wallet"
        );
    }
    gateway.done();
}

/// An index that cannot report its own position must refuse, not deny.
///
/// This is the fail-open the lag gate closes. `chain_head` and `cursor` are
/// both absent on a fresh database, and again while `prepare` replays a chain
/// after a version bump — and an unreadable Postgres looks the same. Folding
/// that into "no lag" made the gateway sign an authoritative "nobody holds
/// this" for every name, cached by wallets for the whole `ENS_TTL`
/// window.
#[tokio::test]
async fn an_index_that_cannot_report_its_position_refuses() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;

    // Drop what the index knows about its own progress, leaving the binding
    // in place: the rows say one thing, the cursor says nothing.
    gateway.store.forget_progress().await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body.refused().contains("cannot report its position"),
        "{body}"
    );
    gateway.done();
}

/// Where the index stands and who holds the handle are one read, so a
/// replay that commits while the gateway reads leaves it refusing, never
/// signing a null over the emptied tables.
#[tokio::test]
async fn a_replay_while_the_gateway_reads_is_refused() {
    let Some(mut gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;
    // The gateway reads on sessions of its own name, so the wait below is
    // its wait and no other session's.
    const GATEWAY: &str = "gateway under test";
    let gateway_pool = PgPool::connect_with(
        (*gateway.pool.connect_options())
            .clone()
            .application_name(GATEWAY),
    )
    .await
    .expect("the gateway's pool");
    gateway.config.store = db::Store::new(gateway_pool);
    let router = gateway.router();
    // The replay holds the handles, and commits once the gateway waits on
    // them.
    let mut replay = gateway.pool.begin().await.expect("a transaction");
    sqlx::query(include_str!("sql/lock_handles.sql"))
        .execute(&mut *replay)
        .await
        .expect("the handles are held");
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let asking = tokio::spawn({
        let (router, call) = (router.clone(), call.clone());
        async move { router.ask(RESOLVER, &call).await }
    });
    gateway.pool.waits_on_a_lock(GATEWAY).await;
    sqlx::query(include_str!("sql/replay_chain.sql"))
        .bind(CHAIN)
        .execute(&mut *replay)
        .await
        .expect("the replay");
    replay.commit().await.expect("the replay commits");

    let (status, body) = asking.await.expect("the answer");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body.refused().contains("cannot report its position"),
        "{body}"
    );
    gateway.done();
}

/// The block gate cannot see a stopped indexer: target and cursor are both
/// its own writes, so they freeze together and lag reads zero for as long as
/// it stays down — signed nulls for bindings made since, and old addresses
/// after a rebind, for the whole `ENS_TTL` each. The report the indexer
/// makes beside the target expires on the indexer's own schedule, and that is
/// what notices.
#[tokio::test]
async fn an_expired_report_is_too_stale_however_small_the_lag() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;
    // Caught up by the block measure, and the report long expired.
    gateway.store.set_chain_target_valid_until(1).await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.refused().contains("expired"), "{body}");
    gateway.done();
}

/// A report whose expiry is out of any duration's range cannot be read as a
/// validity, so the gateway refuses rather than trusts it.
#[tokio::test]
async fn a_report_expiring_out_of_range_is_refused() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;
    gateway
        .store
        .set_chain_target_valid_until(i64::MAX as u64)
        .await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body.refused().contains("cannot report its position"),
        "{body}"
    );
    gateway.done();
}

/// One chain's dead indexer is that chain's problem. Every row the gate reads
/// is keyed by chain, so the refusal reaches the chain whose report expired
/// and no other — the API keeps answering for everyone else. And supervision
/// sees exactly what the gate sees, one row per chain.
#[tokio::test]
async fn a_dead_indexer_on_one_chain_does_not_touch_another() {
    let Some(gateway) = Gateway::open(&[CHAIN, OTHER], 32).await else {
        return;
    };
    let router = gateway.router();
    let here = Address::from([0xbe; 20]);
    gateway.store.bind("alice", here).await;
    let other_store = ChainStore::new(gateway.pool.clone(), OTHER);
    other_store.bind("alice", Address::from([0xed; 20])).await;
    // The other chain's indexer stopped long ago.
    other_store.set_chain_target_valid_until(1).await;

    let dead = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | OTHER as u64),
    );
    let (status, body) = router.ask(RESOLVER, &dead).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body.refused().contains("expired"),
        "refused for the right reason: {body}"
    );

    let alive = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &alive).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&alive), Some(here));

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
    let status: GatewayStatus = serde_json::from_slice(&body).expect("a gateway status");
    let other = status.row(OTHER);
    let chain = status.row(CHAIN);
    assert!(other.stale, "{status:?}");
    assert!(!chain.stale, "{status:?}");
    assert!(other.report_valid_for.is_some_and(|s| s < 0), "{status:?}");
    assert!(chain.report_valid_for.is_some_and(|s| s > 0), "{status:?}");
    gateway.done();
}

/// A chain has only the names its indexer declared. With none declared, the
/// unlabelled name answers under its coin type, and any label at all is a
/// name nobody holds.
#[tokio::test]
async fn a_chain_with_no_declared_name_answers_only_the_unlabelled_name() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let here = Address::from([0xbe; 20]);
    gateway.store.bind("alice", here).await;
    let coin = 0x8000_0000 | CHAIN as u64;

    let plain =
        resolve_call(&["alice", "x"].wire_name(), &["alice", "x"].addr_call(coin));
    let (_, body) = router.ask(RESOLVER, &plain).await;
    assert_eq!(body.verify(&plain), Some(here));

    let labelled = resolve_call(
        &["alice", "x", "local"].wire_name(),
        &["alice", "x", "local"].addr_call(coin),
    );
    let (status, body) = router.ask(RESOLVER, &labelled).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&labelled), None);
    gateway.done();
}

/// The chains served are whatever the store holds. Nothing is configured, so
/// an indexer for a new chain is answered for the first time it commits —
/// with no restart, and no list anywhere to forget it in.
#[tokio::test]
async fn a_chain_that_appears_in_the_store_is_served_without_a_restart() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | OTHER as u64),
    );

    // Not in the store yet: refused unsigned, so a wallet walks on.
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body.refused().contains("serves no chain"),
        "refused for the right reason: {body}"
    );

    // An indexer for it commits its first window and reports.
    let other = ChainStore::new(gateway.pool.clone(), OTHER).writer().await;
    other
        .begin_window()
        .await
        .expect("begin")
        .commit(1)
        .await
        .expect("commit");
    other.set_chain_head(1).await;
    other.set_chain_target(1, Duration::from_secs(120)).await;
    let there = Address::from([0xed; 20]);
    other.store().bind("alice", there).await;

    // The same router, untouched, now answers for it.
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&call), Some(there));
    gateway.done();
}

/// Two indexed chains that share one coin type cannot be told apart by a
/// query, so neither is answered: an unsigned refusal keeps the wallet
/// walking, where a guess would sign the wrong chain's address. Decided per
/// request, where the store is known, rather than at a startup that knows no
/// chains.
#[tokio::test]
async fn two_indexed_chains_on_one_coin_type_are_refused_unsigned() {
    let Some(gateway) = Gateway::open(&[EDEN, TWIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xed; 20])).await;
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000u64 | EDEN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.refused().contains("share"), "{body}");

    // And supervision sees the same verdict, on both halves of the pair.
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let status: GatewayStatus = serde_json::from_slice(&body).expect("a gateway status");
    for id in [EDEN, TWIN] {
        let row = status.row(id);
        assert!(row.ambiguous && row.stale, "{status:?}");
    }
    gateway.done();
}

/// An indexer declares its chain's names before its first window commits,
/// so a labelled query to a chain without a cursor refuses rather than
/// signing a null for a label not declared yet.
#[tokio::test]
async fn a_label_on_a_chain_without_a_cursor_refuses() {
    let Some(gateway) = Gateway::open(&[EDEN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xed; 20])).await;
    gateway.store.forget_progress().await;
    let call = resolve_call(
        &["alice", "x", "eden"].wire_name(),
        &["alice", "x", "eden"].addr_call(0x8000_0000u64 | EDEN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body.refused().contains("cannot report its position"),
        "{body}"
    );
    gateway.done();
}

/// A chain without a cursor (in its first window, or mid-replay) still holds
/// its coin type, so its twin is refused as ambiguous, not answered as the
/// only chain.
#[tokio::test]
async fn a_twin_without_a_cursor_still_makes_the_coin_type_ambiguous() {
    let Some(gateway) = Gateway::open(&[EDEN, TWIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xed; 20])).await;
    ChainStore::new(gateway.pool.clone(), TWIN)
        .forget_progress()
        .await;
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000u64 | EDEN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.refused().contains("share"), "{body}");
    gateway.done();
}

/// The eden testnet resolves, and that it does is the whole point of matching
/// coin types forwards instead of decoding them backwards.
///
/// Eden's chain id is 3735928814 — `0xDEADBFEE`, bit 31 already set — so
/// `0x80000000 | chainId` returns it unchanged. A gateway that decoded the
/// coin type would get 1588445166 and refuse every eden name. Comparing
/// against the chains the store holds gets it right, and the only thing that
/// is genuinely ambiguous — both colliding chains in one store — is refused
/// per request instead.
#[tokio::test]
async fn the_eden_testnet_resolves_despite_its_chain_id() {
    let Some(gateway) = Gateway::open(&[EDEN], 32).await else {
        return;
    };
    let router = gateway.router();
    let owner = Address::from([0xed; 20]);
    gateway.store.bind("alice", owner).await;
    gateway.store.declare_names(&["eden"]).await;

    // Exactly what a wallet on eden sends: `0x80000000 | 3735928814`, which is
    // 3735928814 itself.
    let call = resolve_call(
        &["alice", "x", "eden"].wire_name(),
        &["alice", "x", "eden"].addr_call(0x8000_0000u64 | EDEN as u64),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "eden must resolve: {body}");
    assert_eq!(body.verify(&call), Some(owner));
    gateway.done();
}

/// The legacy shape decodes, and a gateway that does not serve mainnet refuses
/// it UNSIGNED.
///
/// The refusal is the multi-chain invariant: through the mainnet registry
/// coin type 60 names Ethereum mainnet, so a gateway serving only some other
/// chain has nothing to say about it. Saying so with a 5xx keeps the client
/// walking the resolver's `urls`; a signed null would be an authoritative
/// "nobody holds this" and would end that walk at the first gateway asked.
#[tokio::test]
async fn the_legacy_addr_shape_is_refused_unsigned_off_mainnet() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].legacy_addr_call(),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a signed null here would end the client's url walk: {body}"
    );
    gateway.done();
}

/// Bare `addr(node)` asks for the chain of the registry it went through, which
/// the deployment names beside its resolver.
///
/// It is the only query MetaMask sends on a network that carries ENS: on
/// Sepolia it asks Sepolia's registry with coin type 60 and sends on Sepolia.
/// Read as Ethereum mainnet, that query is refused by a deployment that
/// indexes Sepolia, and the wallet shows no resolution for a bound name.
#[tokio::test]
async fn bare_addr_asks_for_the_chain_of_the_gateways_registry() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let in_the_mainnet_registry = gateway.router();
    let in_our_chains_registry = GatewayState::new(Config {
        ens_chain: CHAIN as u64,
        ..gateway.config.clone()
    })
    .router();
    let owner = Address::from([0xbe; 20]);
    gateway.store.bind("alice", owner).await;

    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].legacy_addr_call(),
    );
    let (status, body) = in_our_chains_registry.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.verify(&call), Some(owner));

    // The same question through the mainnet registry is a mainnet one, and
    // this store holds no mainnet.
    let (status, body) = in_the_mainnet_registry.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    body.refused();
    gateway.done();
}

/// What the binary serves: the `/v1` routes and the gateway on one router,
/// with the middleware over the whole of it. The gateway route does not
/// swallow the API's namespace, a `/v1` path that matches nothing is a 404
/// rather than the gateway's "sender is not an address", and CORS is applied
/// once rather than layered twice.
#[tokio::test]
async fn the_merged_router_keeps_both_halves_intact() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let owner = Address::from([0xbe; 20]);
    gateway.store.bind("alice", owner).await;

    let app = usernames_api::Service {
        api: AppState::new(Store::new(gateway.pool.clone())),
        gateway: Some(gateway.config.clone()),
    }
    .router();

    let get = |uri: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .uri(uri)
                    .header("origin", "https://handles.link")
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
    // answer and on a refusal alike: a layer applied inside `AppState::router`
    // would leave the gateway route without `Access-Control-Allow-Origin`, and
    // a browser running the batch gateway in the page would never see the
    // answer.
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        &["alice", "x"].addr_call(0x8000_0000 | CHAIN as u64),
    );
    let uri = format!(
        "{}/{RESOLVER:?}/0x{}.json",
        ROUTE_PREFIX,
        hex::encode(&call)
    );
    let allowed_origin = |response: &Response| {
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
    let refused = get(format!("{}/not-an-address/0x00.json", ROUTE_PREFIX)).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        allowed_origin(&refused).as_deref(),
        Some(&b"*"[..]),
        "a refusal without the CORS header is one a browser cannot even read"
    );
    gateway.done();
}

/// The request names one thing twice — once as a name, once as the node inside
/// the record call — and a caller that lets the two disagree is refused rather
/// than answered. The signature would cover both halves, so an answer here
/// would be true of the name and attributed to the node.
#[tokio::test]
async fn a_node_that_is_not_this_name_is_refused() {
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    gateway.store.bind("alice", Address::from([0xbe; 20])).await;

    let coin = 0x8000_0000 | CHAIN as u64;
    let call = resolve_call(
        &["alice", "x"].wire_name(),
        // The namehash of a different name entirely.
        &["bob", "x"].addr_call(coin),
    );
    let (status, body) = router.ask(RESOLVER, &call).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And the well-formed request it differs from still answers, so the
    // refusal is about the disagreement rather than about the name.
    let ok = resolve_call(&["alice", "x"].wire_name(), &["alice", "x"].addr_call(coin));
    let (status, body) = router.ask(RESOLVER, &ok).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.verify(&ok).is_some());
    gateway.done();
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
    let Some(gateway) = Gateway::open(&[CHAIN], 32).await else {
        return;
    };
    let router = gateway.router();
    let node = ens::Name::decode(b"\x07vitalik\x03eth\x00")
        .expect("a name in wire format")
        .node();

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
            let (status, body) = router.ask(RESOLVER, &call).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
    }
    gateway.done();
}
