//! Hashed identities end to end: mocks with the contracts' event surface emit
//! private and disclosing binds, an unpublish and deposits on anvil; the
//! indexer's own loop indexes them; the read API answers; and no plaintext the
//! chain did not disclose is left anywhere in the store.
//!
//! Skips without `DATABASE_URL`, or without `anvil` on PATH, unless `CI` is
//! set: there a missing one fails. Everything past those checks panics on
//! failure.

#[path = "../../../crates/usernames-core/tests/common/mod.rs"]
mod common;

use std::{
    fmt::Debug,
    future::Future,
    process::Command,
    time::Duration,
};

use alloy::{
    network::Ethereum,
    primitives::{
        address,
        Address,
        B256,
        U256,
    },
    providers::{
        DynProvider,
        PendingTransactionBuilder,
        Provider,
        ProviderBuilder,
    },
    sol_types::SolEvent,
};
use common::{
    MockHandleEscrow,
    MockIdentityRegistry,
    Reply,
};
use libid_contracts::bindings::{
    escrow::HandleEscrow,
    identity::IdentityRegistry,
};
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        AddressHistory,
        AddressResolution,
        HandleAmounts,
        HandleResolution,
        IdResolution,
        SearchResults,
    },
    db::{
        self,
        ChainStore,
        WriterLease,
    },
    indexer,
    nodes::KnownPlatform,
};

/// This suite's chain, apart from every other suite's.
const CHAIN: u64 = 43118;

/// The plaintext the scenario binds. Ids never reach the chain; a handle
/// reaches it only when a bind, a publish or a deposit discloses it.
const ALICE_ID: &str = "4815162342";
const BOB_ID: &str = "1234567890123";
const DAVE_ID: &str = "9081726354";

/// The mocks' topics are the bindings' topics, for every event the indexer
/// decodes: a mock that drifted from the contracts would test nothing.
#[test]
fn the_mocks_emit_the_contracts_topics() {
    macro_rules! same_topics {
        ($mock:ident = $binding:ident: $($event:ident),+) => {$(
            assert_eq!(
                $mock::$event::SIGNATURE_HASH,
                $binding::$event::SIGNATURE_HASH,
                stringify!($event)
            );
        )+};
    }
    same_topics!(MockIdentityRegistry = IdentityRegistry:
        IdentityBound, HandlePublished, HandleRetired, HandleUnpublished,
        ProofVerifierConfigured, CeremonyBound, BindFeePaid);
    same_topics!(MockHandleEscrow = HandleEscrow:
        Deposited, DepositedToHandle, Forwarded, Claimed, Refunded);
}

use MockIdentityRegistry::Bind;

/// Skip the test, except under CI, where missing infrastructure fails it.
fn skip<T>(missing: &str) -> Option<T> {
    assert!(
        std::env::var_os("CI").is_none(),
        "{missing}: CI runs this suite against Postgres and anvil"
    );
    eprintln!("skipping: {missing}");
    None
}

type Registry = MockIdentityRegistry::MockIdentityRegistryInstance<DynProvider>;
type Escrow = MockHandleEscrow::MockHandleEscrowInstance<DynProvider>;

/// One test's chain: its own anvil with both doubles deployed, and the
/// indexer's loop over them.
struct Harness {
    chain: u64,
    pool: PgPool,
    store: ChainStore,
    provider: DynProvider,
    registry: Registry,
    escrow: Escrow,
    cancel: CancellationToken,
    indexer: JoinHandle<()>,
    writer: WriterLease,
}

impl Harness {
    /// A fresh `chain`, indexed; `None` when the suite skips.
    async fn open(chain: u64) -> Option<Self> {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return skip("DATABASE_URL not set");
        };
        if Command::new("anvil").arg("--version").output().is_err() {
            return skip("anvil not on PATH");
        }
        let pool = db::connect_and_migrate(&url).await.expect("database");
        common::clear_chain(&pool, chain as i64).await;
        let store = ChainStore::new(pool.clone(), chain as i64);

        let provider = ProviderBuilder::new()
            .connect_anvil_with_wallet_and_config(|anvil| anvil.chain_id(chain))
            .expect("anvil spawns");
        provider
            .client()
            .set_poll_interval(Duration::from_millis(10));
        let provider = provider.erased();
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
                    poll_interval: Duration::from_millis(10),
                    max_block_range: 100,
                    start_block: None,
                    stale_after: Duration::from_secs(120),
                },
            )
            .run(cancel.clone()),
        );

        Some(Self {
            chain,
            pool,
            store,
            provider,
            registry,
            escrow,
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

    /// The answer to one GET on the read API, scoped to this chain.
    async fn get<T: DeserializeOwned + Debug>(&self, path: &str) -> T {
        let reply: Reply<T> = common::get_on(&self.pool, self.chain as i64, path).await;
        reply.answer()
    }

    /// Mine one transaction and wait for the indexer to commit it.
    async fn mine(
        &self,
        send: impl Future<
            Output = alloy::contract::Result<PendingTransactionBuilder<Ethereum>>,
        >,
    ) {
        send.await.unwrap().watch().await.unwrap();
        let latest = self.provider.get_block_number().await.expect("latest");
        common::wait_for_cursor(&self.store, latest).await;
    }

    /// Every column holding text, bytes or JSON on this chain where `needle`
    /// is found: what a privacy scan reads.
    async fn where_found(&self, needle: &str) -> Vec<String> {
        let columns: Vec<(String, String, String, bool)> = sqlx::query_as(
            "SELECT c.table_name, c.column_name, c.data_type,
                    EXISTS (SELECT 1 FROM information_schema.columns k
                            WHERE k.table_schema = 'names'
                              AND k.table_name = c.table_name
                              AND k.column_name = 'chain_id')
             FROM information_schema.columns c
             JOIN information_schema.tables t
               ON t.table_schema = c.table_schema AND t.table_name = c.table_name
             WHERE c.table_schema = 'names' AND t.table_type = 'BASE TABLE'
               AND c.data_type IN ('text', 'bytea', 'jsonb', 'ARRAY')",
        )
        .fetch_all(&self.pool)
        .await
        .expect("columns");
        let mut found = Vec::new();
        for (table, column, kind, chained) in columns {
            let test = if kind == "bytea" {
                format!(
                    "(position(convert_to(lower($1), 'UTF8') in {column}) > 0 \
                     OR position(convert_to($1, 'UTF8') in {column}) > 0)"
                )
            } else {
                format!("lower({column}::text) LIKE '%' || lower($1) || '%'")
            };
            let scope = if chained { " AND chain_id = $2" } else { "" };
            let statement =
                format!("SELECT count(*) FROM names.{table} WHERE {test}{scope}");
            let mut query = sqlx::query_scalar::<_, i64>(&statement).bind(needle);
            if chained {
                query = query.bind(self.chain as i64);
            }
            if query.fetch_one(&self.pool).await.expect("scan") > 0 {
                found.push(format!("{table}.{column}"));
            }
        }
        found
    }
}

/// A private bind of `id` to `handle` on X with nothing to withdraw or
/// retire.
fn x_bind(holder: Address, id: &str, handle: &str, observed_at: u64) -> Bind {
    let digest = B256::from(U256::from(observed_at));
    common::mock_bind(
        holder,
        KnownPlatform::X,
        id,
        handle,
        observed_at,
        false,
        digest,
    )
}

#[tokio::test]
async fn private_binds_stay_private_and_disclosures_stay_shown() {
    let Some(h) = Harness::open(CHAIN).await else {
        return;
    };
    let (registry, escrow) = (&h.registry, &h.escrow);

    let x = KnownPlatform::X;
    let (alice, bob, dave) = (
        Address::repeat_byte(0xA1),
        Address::repeat_byte(0xB0),
        Address::repeat_byte(0xD0),
    );
    let alice_node = x.handle_node("Alice_1").expect("a handle");
    let alice_id = x.id_node(ALICE_ID).expect("an id");

    // 1. A private bind of @Alice_1: nodes only, resolved by hashing here,
    //    with no text to show.
    h.mine(
        registry
            .emitBind(x_bind(alice, ALICE_ID, "alice_1", 1000))
            .send(),
    )
    .await;
    let resolved: HandleResolution = h.get("/v1/resolve/handle/x/Alice_1").await;
    assert_eq!(resolved.handle_node, alice_node);
    assert_eq!(resolved.bindings[0].owner, alice);
    assert_eq!(resolved.bindings[0].id_node, alice_id);
    assert_eq!(resolved.bindings[0].handle, None);
    let by_id: IdResolution = h.get(&format!("/v1/resolve/id-node/{alice_id}")).await;
    assert_eq!(by_id.bindings[0].owner, alice);
    assert_eq!(by_id.bindings[0].handle, None);
    let found: SearchResults = h.get("/v1/search?q=alice").await;
    assert!(found.hits.is_empty(), "{found:?}");

    // 2. A disclosing bind: shown on the address and found by search.
    let disclosing = Bind {
        handle: "alice_1".into(),
        ..x_bind(alice, ALICE_ID, "alice_1", 2000)
    };
    h.mine(registry.emitBind(disclosing).send()).await;
    let theirs: AddressResolution = h.get(&format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(theirs.identities.len(), 1, "{theirs:?}");
    assert_eq!(theirs.identities[0].handle.as_deref(), Some("alice_1"));
    assert!(theirs.identities[0].published);
    let found: SearchResults = h.get("/v1/search?q=alice").await;
    let hits: Vec<&str> = found.hits.iter().map(|hit| hit.handle.as_str()).collect();
    assert_eq!(hits, ["alice_1"]);

    // 3. Withdrawn: no longer published, the text still shown.
    h.mine(registry.emitHandleUnpublished(alice, x.id()).send())
        .await;
    let theirs: AddressResolution = h.get(&format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(theirs.identities[0].handle.as_deref(), Some("alice_1"));
    assert!(!theirs.identities[0].published);
    let found: SearchResults = h.get("/v1/search?q=alice").await;
    let hits: Vec<(&str, bool)> = found
        .hits
        .iter()
        .map(|hit| (hit.handle.as_str(), hit.published))
        .collect();
    assert_eq!(hits, [("alice_1", false)]);
    let resolved: HandleResolution = h.get("/v1/resolve/handle/x/alice_1").await;
    assert_eq!(resolved.bindings[0].handle.as_deref(), Some("alice_1"));
    assert_eq!(
        common::disclosed(&h.pool, h.chain as i64, alice_node)
            .await
            .as_deref(),
        Some("alice_1")
    );
    let history: serde_json::Value = h.get(&format!("/v1/history/address/{alice}")).await;
    assert!(history.to_string().contains("alice_1"), "{history}");

    // 4. A public deposit to @Bob_2, then Bob binds it privately: the
    //    disclosed handle maps to his id node. A private deposit names only
    //    a node, and no platform.
    let native = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
    let payer = Address::repeat_byte(0xC0);
    let bob_node = x.handle_node("bob_2").expect("a handle");
    let bob_id = x.id_node(BOB_ID).expect("an id");
    let carol_node = x.handle_node("carol_3").expect("a handle");
    let (round, hundred, five) = (U256::ZERO, U256::from(100u64), U256::from(5u64));
    let to_bob = escrow.emitDepositedToHandle(
        bob_node,
        native,
        payer,
        payer,
        round,
        hundred,
        x.id(),
        "Bob_2".into(),
    );
    h.mine(to_bob.send()).await;
    let to_carol = escrow.emitDeposited(carol_node, native, payer, payer, round, five);
    h.mine(to_carol.send()).await;
    h.mine(registry.emitBind(x_bind(bob, BOB_ID, "bob_2", 3000)).send())
        .await;
    // Dave binds privately and is never disclosed.
    h.mine(
        registry
            .emitBind(x_bind(dave, DAVE_ID, "dave_4", 4000))
            .send(),
    )
    .await;

    let mapping: Vec<(String, B256, Option<B256>, Option<Address>)> = sqlx::query_as(
        "SELECT handle, handle_node, id_node, holder FROM names.handle_identities
         WHERE chain_id = $1 ORDER BY handle",
    )
    .bind(CHAIN as i64)
    .fetch_all(&h.pool)
    .await
    .expect("the mapping");
    assert_eq!(
        mapping,
        [
            (
                "alice_1".to_string(),
                alice_node,
                Some(alice_id),
                Some(alice)
            ),
            ("bob_2".to_string(), bob_node, Some(bob_id), Some(bob)),
        ],
        "carol and dave were never disclosed"
    );
    let by_node: HandleResolution =
        h.get(&format!("/v1/resolve/handle-node/{bob_node}")).await;
    assert_eq!(by_node.bindings[0].handle.as_deref(), Some("bob_2"));
    assert_eq!(by_node.bindings[0].id_node, bob_id);
    let by_id: IdResolution = h.get(&format!("/v1/resolve/id-node/{bob_id}")).await;
    assert_eq!(by_id.bindings[0].handle.as_deref(), Some("bob_2"));
    let carols: HandleAmounts = h.get(&format!("/v1/escrow/node/{carol_node}")).await;
    assert_eq!(carols.handle.platform_id, None, "{carols:?}");
    assert_eq!(carols.handle.handle, None);
    assert_eq!(carols.amounts[0].amount, five);
    let bobs: HandleAmounts = h.get("/v1/escrow/handle/x/BOB_2").await;
    assert_eq!(bobs.handle.platform_id, Some(x.id()));
    assert_eq!(bobs.handle.handle.as_deref(), Some("bob_2"));
    let payers: AddressHistory = h.get(&format!("/v1/history/address/{payer}")).await;
    assert_eq!(payers.entries.len(), 2, "{payers:?}");

    // 5. The privacy scan: no id anywhere, no undisclosed handle anywhere,
    //    and the disclosed ones only in the disclosures table.
    for id in [ALICE_ID, BOB_ID, DAVE_ID] {
        assert_eq!(h.where_found(id).await, Vec::<String>::new(), "id {id}");
    }
    for handle in ["carol_3", "dave_4"] {
        assert_eq!(
            h.where_found(handle).await,
            Vec::<String>::new(),
            "{handle}"
        );
    }
    for handle in ["alice_1", "bob_2"] {
        assert_eq!(
            h.where_found(handle).await,
            ["disclosures.handle"],
            "{handle}"
        );
    }
    let bob_history: serde_json::Value =
        h.get(&format!("/v1/history/address/{bob}")).await;
    let bob_history = bob_history.to_string();
    for secret in [BOB_ID, "alice_1", "carol_3", "dave_4"] {
        assert!(!bob_history.contains(secret), "{secret}: {bob_history}");
    }
    h.close().await;
}
