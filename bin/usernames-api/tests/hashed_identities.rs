//! Hashed identities end to end: mocks with the contracts' event surface emit
//! private binds, disclosing binds, an unpublish and deposits on anvil; the
//! indexer's own loop indexes them; the read API and the ENS gateway answer;
//! and no plaintext the chain did not disclose is left anywhere in the store.
//!
//! Skips without `DATABASE_URL`, or without `anvil` on PATH. Everything past
//! those checks panics on failure.

use std::{
    process::Command,
    sync::Arc,
    time::Duration,
};

use alloy::{
    primitives::{
        address,
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::{
        DynProvider,
        Provider,
        ProviderBuilder,
    },
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{
        SolEvent,
        SolValue,
    },
};
use axum::{
    body::Body,
    http::Request,
    Router,
};
use http_body_util::BodyExt;
use libid_contracts::bindings::{
    escrow::HandleEscrow,
    identity::IdentityRegistry,
};
use libid_signer::ManagedSigner;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tokio::{
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use usernames_api::ens::{
    Config,
    GatewayResponse,
    GatewayState,
};
use usernames_core::{
    api::{
        self,
        model::{
            AddressHistory,
            AddressResolution,
            HandleAmounts,
            HandleResolution,
            IdResolution,
            Role,
            SearchResults,
        },
    },
    db::{
        self,
        ChainStore,
        Store,
        WriterLease,
    },
    ens::{
        CoinType,
        DEFAULT_DOMAIN,
    },
    indexer,
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    addr_call,
    resolve_call,
    wire_name,
};

sol!(
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    MockIdentityRegistry,
    "../../contracts/MockIdentityRegistry.json"
);

sol!(
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    MockHandleEscrow,
    "../../contracts/MockHandleEscrow.json"
);

/// This suite's chain, apart from every other suite's.
const CHAIN: u64 = 43118;
const RESOLVER: Address = Address::new([0xaa; 20]);
const SIGNER_KEY: &str =
    "0x00000000000000000000000000000000000000000000000000000000000a11ce";

/// The plaintext the scenario binds. Ids never reach the chain; a handle
/// reaches it only when a bind, a publish or a deposit discloses it.
const ALICE_ID: &str = "4815162342";
const BOB_ID: &str = "1234567890123";
const DAVE_ID: &str = "9081726354";

/// The mock's topics are the bindings' topics, for every event the indexer
/// decodes: a mock that drifted from the contracts would test nothing.
#[test]
fn the_mocks_emit_the_contracts_topics() {
    let pairs = [
        (
            MockIdentityRegistry::IdentityBound::SIGNATURE_HASH,
            IdentityRegistry::IdentityBound::SIGNATURE_HASH,
        ),
        (
            MockIdentityRegistry::HandlePublished::SIGNATURE_HASH,
            IdentityRegistry::HandlePublished::SIGNATURE_HASH,
        ),
        (
            MockIdentityRegistry::HandleRetired::SIGNATURE_HASH,
            IdentityRegistry::HandleRetired::SIGNATURE_HASH,
        ),
        (
            MockIdentityRegistry::HandleUnpublished::SIGNATURE_HASH,
            IdentityRegistry::HandleUnpublished::SIGNATURE_HASH,
        ),
        (
            MockIdentityRegistry::ProofVerifierConfigured::SIGNATURE_HASH,
            IdentityRegistry::ProofVerifierConfigured::SIGNATURE_HASH,
        ),
        (
            MockIdentityRegistry::CeremonyBound::SIGNATURE_HASH,
            IdentityRegistry::CeremonyBound::SIGNATURE_HASH,
        ),
        (
            MockIdentityRegistry::BindFeePaid::SIGNATURE_HASH,
            IdentityRegistry::BindFeePaid::SIGNATURE_HASH,
        ),
        (
            MockHandleEscrow::Deposited::SIGNATURE_HASH,
            HandleEscrow::Deposited::SIGNATURE_HASH,
        ),
        (
            MockHandleEscrow::DepositedToHandle::SIGNATURE_HASH,
            HandleEscrow::DepositedToHandle::SIGNATURE_HASH,
        ),
        (
            MockHandleEscrow::Forwarded::SIGNATURE_HASH,
            HandleEscrow::Forwarded::SIGNATURE_HASH,
        ),
        (
            MockHandleEscrow::Claimed::SIGNATURE_HASH,
            HandleEscrow::Claimed::SIGNATURE_HASH,
        ),
        (
            MockHandleEscrow::Refunded::SIGNATURE_HASH,
            HandleEscrow::Refunded::SIGNATURE_HASH,
        ),
    ];
    for (i, (mock, binding)) in pairs.into_iter().enumerate() {
        assert_eq!(mock, binding, "event {i}");
    }
}

use MockIdentityRegistry::Bind;

type Registry = MockIdentityRegistry::MockIdentityRegistryInstance<DynProvider>;
type Escrow = MockHandleEscrow::MockHandleEscrowInstance<DynProvider>;

/// One test's chain: its own anvil with both doubles deployed, the indexer's
/// loop over them, and the read API and the gateway over the store.
struct Harness {
    chain: u64,
    pool: PgPool,
    store: ChainStore,
    provider: DynProvider,
    registry: Registry,
    escrow: Escrow,
    gateway: Router,
    cancel: CancellationToken,
    indexer: JoinHandle<()>,
    writer: WriterLease,
}

impl Harness {
    /// A fresh `chain`, indexed; `None` when the suite skips.
    async fn open(chain: u64) -> Option<Self> {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        };
        if Command::new("anvil").arg("--version").output().is_err() {
            eprintln!("skipping: anvil not on PATH");
            return None;
        }
        let pool = db::connect_and_migrate(&url).await.expect("database");
        for statement in [
            db::CLEAR_CHAIN,
            db::CLEAR_CHAIN_METADATA,
            db::CLEAR_CHAIN_NAMES,
        ] {
            sqlx::query(statement)
                .bind(chain as i64)
                .execute(&pool)
                .await
                .expect("cleanup");
        }
        let store = ChainStore::new(pool.clone(), chain as i64);

        let provider = ProviderBuilder::new()
            .connect_anvil_with_wallet_and_config(|anvil| anvil.chain_id(chain))
            .expect("anvil spawns")
            .erased();
        let registry = MockIdentityRegistry::deploy(provider.clone())
            .await
            .expect("deploy the registry");
        let escrow = MockHandleEscrow::deploy(provider.clone(), *registry.address())
            .await
            .expect("deploy the escrow");

        let writer = store.acquire_writer().await.expect("writer lease");
        store
            .prepare(&writer, *registry.address(), Some(*escrow.address()))
            .await
            .expect("prepare");
        let cancel = CancellationToken::new();
        let indexer = tokio::spawn(
            indexer::Indexer::new(
                store.clone(),
                provider.clone(),
                indexer::IndexerConfig {
                    contract: *registry.address(),
                    escrow: Some(*escrow.address()),
                    confirmations: 0,
                    poll_interval: Duration::from_millis(100),
                    max_block_range: 100,
                    start_block: None,
                    stale_after: Duration::from_secs(120),
                },
            )
            .run(cancel.clone()),
        );

        let gateway = usernames_api::ens::router(GatewayState::new(Config {
            domain: DEFAULT_DOMAIN.parse().expect("domain"),
            resolver: RESOLVER,
            ens_chain: 1,
            store: Store::new(pool.clone()),
            ttl: Duration::from_secs(300),
            max_lag_blocks: 32,
            signer: Arc::new(ManagedSigner::Local(
                SIGNER_KEY.parse::<PrivateKeySigner>().expect("key"),
            )),
        }));

        Some(Self {
            chain,
            pool,
            store,
            provider,
            registry,
            escrow,
            gateway,
            cancel,
            indexer,
            writer,
        })
    }

    /// Stop the loop and release the lease.
    async fn close(self) {
        self.cancel.cancel();
        let _ = self.indexer.await;
        self.writer.release().await.expect("the lease releases");
    }

    /// One GET on the read API, scoped to this chain.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> (u16, Result<T, String>) {
        let separator = if path.contains('?') { '&' } else { '?' };
        let response = api::router(api::AppState::new(Store::new(self.pool.clone())))
            .oneshot(
                Request::builder()
                    .uri(format!("{path}{separator}chain={}", self.chain))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = if status == 200 {
            Ok(serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{e}: {text}")))
        } else {
            Err(text)
        };
        (status, body)
    }

    /// The answer to a GET that must succeed.
    async fn answer<T: DeserializeOwned>(&self, path: &str) -> T {
        let (status, body) = self.get(path).await;
        body.unwrap_or_else(|refusal| panic!("{path}: {status}: {refusal}"))
    }

    /// The raw body of a GET, for scanning.
    async fn raw(&self, path: &str) -> String {
        let (_, body) = self.get::<serde_json::Value>(path).await;
        body.map(|value| value.to_string())
            .unwrap_or_else(|refusal| refusal)
    }

    /// The address the gateway signs for `labels` on this chain.
    async fn ens_addr(&self, labels: &[&str]) -> Option<Address> {
        let call = resolve_call(
            &wire_name(labels),
            &addr_call(labels, CoinType::of_chain(self.chain).raw().to::<u64>()),
        );
        let uri = format!("/{RESOLVER}/0x{}.json", hex::encode(&call));
        let response = self
            .gateway
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .expect("gateway");
        assert_eq!(response.status(), 200, "the gateway answers");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let reply: GatewayResponse = serde_json::from_slice(&bytes).expect("an answer");
        let (result, _, _) =
            <(Bytes, u64, Bytes)>::abi_decode_params(&reply.data).expect("the blob");
        let record = Bytes::abi_decode(&result).expect("addr's bytes");
        (!record.is_empty()).then(|| Address::from_slice(&record))
    }

    /// Mine one `bind`'s events and wait for the indexer to commit them.
    async fn bind(&self, bind: Bind) {
        self.registry
            .emitBind(bind)
            .send()
            .await
            .unwrap()
            .watch()
            .await
            .unwrap();
        self.caught_up().await;
    }

    /// Wait for the indexer to commit through the chain's latest block.
    async fn caught_up(&self) {
        let latest = self.provider.get_block_number().await.expect("latest");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let cursor = self.store.cursor().await.expect("cursor");
            if cursor.is_some_and(|c| c >= latest) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "cursor {cursor:?} behind {latest}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// A disclosure's (source, withdrawn), when the node has one.
    async fn disclosure(&self, handle_node: B256) -> Option<(String, bool)> {
        sqlx::query_as(
            "SELECT source, withdrawn FROM names.disclosures
             WHERE chain_id = $1 AND handle_node = $2",
        )
        .bind(self.chain as i64)
        .bind(handle_node)
        .fetch_optional(&self.pool)
        .await
        .expect("a disclosure read")
    }
}

/// A private bind of `id` to `handle` on X with nothing to withdraw or
/// retire; a scenario sets what its bind finds.
fn x_bind(holder: Address, id: &str, handle: &str, observed_at: u64) -> Bind {
    let x = KnownPlatform::X;
    Bind {
        holder,
        idNode: x.id_node(id).expect("an id"),
        handleNode: x.handle_node(handle).expect("a handle"),
        platformId: x.id(),
        observedAt: observed_at,
        ceremonyVersion: 1,
        takenFrom: Address::ZERO,
        retiredNode: B256::ZERO,
        retiredHolder: Address::ZERO,
        retiredNamed: false,
        handle: String::new(),
        authorizationDigest: B256::from(U256::from(observed_at)),
    }
}

/// An address history as (kind, roles, handle node), newest first.
fn entries(history: &AddressHistory) -> Vec<(String, Vec<Role>, Option<B256>)> {
    history
        .entries
        .iter()
        .map(|e| {
            let kind = serde_json::to_value(&e.event).expect("an event")["kind"]
                .as_str()
                .expect("a kind")
                .to_string();
            (
                kind,
                e.roles.clone(),
                e.handle.as_ref().map(|h| h.handle_node),
            )
        })
        .collect()
}

/// Every place the store keeps text, bytes or JSON, and how many rows of it
/// hold `needle`: what a privacy scan reads.
async fn where_found(pool: &PgPool, needle: &str) -> Vec<String> {
    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.table_name, c.column_name, c.data_type
         FROM information_schema.columns c
         JOIN information_schema.tables t
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name
         WHERE c.table_schema = 'names' AND t.table_type = 'BASE TABLE'
           AND c.data_type IN ('text', 'bytea', 'jsonb', 'ARRAY')",
    )
    .fetch_all(pool)
    .await
    .expect("columns");
    let mut found = Vec::new();
    for (table, column, kind) in columns {
        let test = if kind == "bytea" {
            format!(
                "position(convert_to(lower($1), 'UTF8') in {column}) > 0 \
                 OR position(convert_to($1, 'UTF8') in {column}) > 0"
            )
        } else {
            format!("lower({column}::text) LIKE '%' || lower($1) || '%'")
        };
        let hits: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM names.{table} WHERE {test}"
        ))
        .bind(needle)
        .fetch_one(pool)
        .await
        .expect("scan");
        if hits > 0 {
            found.push(format!("{table}.{column}"));
        }
    }
    found
}

#[tokio::test]
async fn private_binds_stay_private_and_disclosures_show_until_withdrawn() {
    let Some(h) = Harness::open(CHAIN).await else {
        return;
    };
    let (pool, registry, escrow) = (&h.pool, &h.registry, &h.escrow);

    let x = KnownPlatform::X;
    let (alice, bob, dave) = (
        Address::repeat_byte(0xA1),
        Address::repeat_byte(0xB0),
        Address::repeat_byte(0xD0),
    );
    let alice_node = x.handle_node("Alice_1").expect("a handle");
    let alice_id = x.id_node(ALICE_ID).expect("an id");

    // 1. A private bind of @Alice_1: nodes only.
    registry
        .emitIdentityBound(alice, alice_id, alice_node, x.id(), 1000, 1)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    h.caught_up().await;

    // 2. Hashed here, resolved by node; the text is nowhere to show.
    let resolved: HandleResolution = h.answer("/v1/resolve/handle/x/Alice_1").await;
    assert_eq!(resolved.handle_node, alice_node);
    assert_eq!(resolved.bindings[0].owner, alice);
    assert_eq!(resolved.bindings[0].id_node, alice_id);
    assert_eq!(resolved.bindings[0].handle, None);
    let by_id: IdResolution = h.answer(&format!("/v1/resolve/id/x/{ALICE_ID}")).await;
    assert_eq!(by_id.bindings[0].owner, alice);
    assert_eq!(by_id.bindings[0].handle, None);
    let found: SearchResults = h.answer("/v1/search?q=alice").await;
    assert!(found.hits.is_empty(), "{found:?}");
    // The gateway resolves a private binding, as `resolveHandle` does.
    assert_eq!(h.ens_addr(&["alice-1", "x"]).await, Some(alice));

    // 3. A disclosing bind: shown on the address and found by search.
    registry
        .emitDisclosingBind(
            alice,
            alice_id,
            alice_node,
            x.id(),
            2000,
            1,
            "alice_1".into(),
        )
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    h.caught_up().await;
    let theirs: AddressResolution =
        h.answer(&format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(theirs.identities.len(), 1, "{theirs:?}");
    assert_eq!(theirs.identities[0].handle.as_deref(), Some("alice_1"));
    assert!(theirs.identities[0].published);
    let found: SearchResults = h.answer("/v1/search?q=alice").await;
    let hits: Vec<&str> = found.hits.iter().map(|hit| hit.handle.as_str()).collect();
    assert_eq!(hits, ["alice_1"]);
    assert_eq!(h.disclosure(alice_node).await, Some(("bind".into(), false)));

    // 4. Withdrawn: hidden again, the row kept and marked.
    registry
        .emitHandleUnpublished(alice, x.id())
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    h.caught_up().await;
    let theirs: AddressResolution =
        h.answer(&format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(theirs.identities[0].handle, None);
    assert!(!theirs.identities[0].published);
    let found: SearchResults = h.answer("/v1/search?q=alice").await;
    assert!(found.hits.is_empty(), "{found:?}");
    let resolved: HandleResolution = h.answer("/v1/resolve/handle/x/alice_1").await;
    assert_eq!(resolved.bindings[0].handle, None);
    assert_eq!(
        h.disclosure(alice_node).await,
        Some(("bind".into(), true)),
        "the disclosure stays, withdrawn"
    );
    let history = h.raw(&format!("/v1/history/address/{alice}")).await;
    assert!(!history.contains("alice_1"), "{history}");

    // 5. The gateway still resolves the name, through the SHA node.
    assert_eq!(h.ens_addr(&["alice-1", "x"]).await, Some(alice));
    assert_eq!(h.ens_addr(&["nobody", "x"]).await, None);

    // 6. A public deposit to @Bob_2, then Bob binds it privately: the
    //    disclosed handle maps to his id node. A private deposit names only
    //    a node, and no platform.
    let native = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
    let payer = Address::repeat_byte(0xC0);
    let bob_node = x.handle_node("bob_2").expect("a handle");
    let bob_id = x.id_node(BOB_ID).expect("an id");
    let carol_node = x.handle_node("carol_3").expect("a handle");
    escrow
        .emitDepositedToHandle(
            bob_node,
            native,
            payer,
            payer,
            U256::ZERO,
            U256::from(100u64),
            x.id(),
            "Bob_2".into(),
        )
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    escrow
        .emitDeposited(
            carol_node,
            native,
            payer,
            payer,
            U256::ZERO,
            U256::from(5u64),
        )
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    registry
        .emitIdentityBound(bob, bob_id, bob_node, x.id(), 3000, 1)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    // Dave binds privately and is never disclosed.
    registry
        .emitIdentityBound(
            dave,
            x.id_node(DAVE_ID).expect("an id"),
            x.handle_node("dave_4").expect("a handle"),
            x.id(),
            4000,
            1,
        )
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    h.caught_up().await;

    let mapping: Vec<(String, B256, Option<B256>, Option<Address>)> = sqlx::query_as(
        "SELECT handle, handle_node, id_node, holder FROM names.handle_identities
         WHERE chain_id = $1 ORDER BY handle",
    )
    .bind(CHAIN as i64)
    .fetch_all(pool)
    .await
    .expect("the mapping");
    assert_eq!(
        mapping,
        [("bob_2".to_string(), bob_node, Some(bob_id), Some(bob))],
        "alice_1 is withdrawn, carol and dave were never disclosed"
    );
    let by_node: HandleResolution = h
        .answer(&format!("/v1/resolve/handle-node/{bob_node}"))
        .await;
    assert_eq!(by_node.bindings[0].handle.as_deref(), Some("bob_2"));
    assert_eq!(by_node.bindings[0].id_node, bob_id);
    let by_id: IdResolution = h.answer(&format!("/v1/resolve/id-node/{bob_id}")).await;
    assert_eq!(by_id.bindings[0].handle.as_deref(), Some("bob_2"));
    let carols: HandleAmounts = h.answer(&format!("/v1/escrow/node/{carol_node}")).await;
    assert_eq!(carols.handle.platform_id, None, "{carols:?}");
    assert_eq!(carols.handle.handle, None);
    assert_eq!(carols.amounts[0].amount, U256::from(5u64));
    let bobs: HandleAmounts = h.answer("/v1/escrow/handle/x/BOB_2").await;
    assert_eq!(bobs.handle.platform_id, Some(x.id()));
    assert_eq!(bobs.handle.handle.as_deref(), Some("bob_2"));
    let payers: AddressHistory = h.answer(&format!("/v1/history/address/{payer}")).await;
    assert_eq!(payers.entries.len(), 2, "{payers:?}");

    // 7. The privacy scan: no id anywhere, no undisclosed handle anywhere,
    //    and the disclosed ones only in the disclosures table.
    for id in [ALICE_ID, BOB_ID, DAVE_ID] {
        assert_eq!(where_found(pool, id).await, Vec::<String>::new(), "id {id}");
    }
    for handle in ["carol_3", "dave_4"] {
        assert_eq!(
            where_found(pool, handle).await,
            Vec::<String>::new(),
            "{handle}"
        );
    }
    for handle in ["alice_1", "bob_2"] {
        assert_eq!(
            where_found(pool, handle).await,
            ["disclosures.handle"],
            "{handle}"
        );
    }
    let bob_history = h.raw(&format!("/v1/history/address/{bob}")).await;
    for secret in [BOB_ID, "alice_1", "carol_3", "dave_4"] {
        assert!(!bob_history.contains(secret), "{secret}: {bob_history}");
    }
    h.close().await;
}

/// A retirement is in the history of the address that held the handle, which
/// the event names, whoever's bind retired it.
#[tokio::test]
async fn a_retired_handle_is_in_the_history_of_the_address_that_held_it() {
    let Some(h) = Harness::open(43119).await else {
        return;
    };
    let x = KnownPlatform::X;
    let node = |handle: &str| x.handle_node(handle).expect("a handle");
    let (alice, bob) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));

    // Alice binds @kim1, disclosing it, then renames to @kim2: her bind
    // retires @kim1 and withdraws her name for it.
    h.bind(Bind {
        handle: "kim1".into(),
        ..x_bind(alice, "777", "kim1", 1_000)
    })
    .await;
    h.bind(Bind {
        retiredNode: node("kim1"),
        retiredHolder: alice,
        retiredNamed: true,
        handle: "kim2".into(),
        ..x_bind(alice, "777", "kim2", 2_000)
    })
    .await;
    // The account moves to Bob's address and renames to @kim3, privately:
    // @kim2 retires from Alice, and her name for it goes.
    h.bind(Bind {
        retiredNode: node("kim2"),
        retiredHolder: alice,
        retiredNamed: true,
        ..x_bind(bob, "777", "kim3", 3_000)
    })
    .await;

    let history: AddressHistory = h.answer(&format!("/v1/history/address/{alice}")).await;
    let (holder, held, moved) = (
        vec![Role::Holder],
        vec![Role::PreviousHandleHolder],
        vec![Role::PreviousIdHolder],
    );
    let row = |kind: &str, roles: &Vec<Role>, handle: &str| {
        (kind.to_string(), roles.clone(), Some(node(handle)))
    };
    assert_eq!(
        entries(&history),
        [
            row("identity_bound", &moved, "kim3"),
            row("handle_unpublished", &holder, "kim2"),
            row("handle_retired", &held, "kim2"),
            row("ceremony_bound", &holder, "kim2"),
            row("handle_published", &holder, "kim2"),
            row("identity_bound", &holder, "kim2"),
            row("handle_unpublished", &holder, "kim1"),
            row("handle_retired", &held, "kim1"),
            row("ceremony_bound", &holder, "kim1"),
            row("handle_published", &holder, "kim1"),
            row("identity_bound", &holder, "kim1"),
        ]
    );
    // Bob never held @kim2: his history is his own bind.
    let history: AddressHistory = h.answer(&format!("/v1/history/address/{bob}")).await;
    assert_eq!(
        entries(&history),
        [
            row("ceremony_bound", &holder, "kim3"),
            row("identity_bound", &holder, "kim3"),
        ]
    );
    h.close().await;
}
