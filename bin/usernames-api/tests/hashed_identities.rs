//! Hashed identities end to end: the libid contracts on anvil take private
//! and disclosing binds, an unpublish and deposits; the indexer's own loop
//! indexes them; the read API answers; and no plaintext the chain did not
//! disclose is left anywhere in the store.
//!
//! Skips without `DATABASE_URL`, or without `anvil` on PATH, unless `CI` is
//! set: there a missing one fails. Everything past those checks panics on
//! failure.

use std::{
    fmt::Debug,
    time::Duration,
};

use alloy::{
    primitives::{
        Address,
        U256,
    },
    providers::Provider,
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
        Superseded,
        WriterLease,
    },
    indexer,
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    anvil_available,
    database_url,
    libid::{
        Disclose,
        Handle,
        Libid,
        Node,
        Proof,
    },
    ChainWatch,
    IdNode,
    Reads,
    Reply,
};

/// This suite's chain, apart from every other suite's.
const CHAIN: u64 = 43118;

/// The plaintext the scenario binds. Ids never reach the chain; a handle
/// reaches it only when a bind, a publish or a deposit discloses it.
const ALICE_ID: &str = "4815162342";
const BOB_ID: &str = "1234567890123";
const DAVE_ID: &str = "9081726354";

/// One test's chain: its own anvil with the libid contracts deployed, and
/// the indexer's loop over them.
struct Harness {
    chain: u64,
    pool: PgPool,
    store: ChainStore,
    node: Node,
    libid: Libid,
    cancel: CancellationToken,
    indexer: JoinHandle<Result<(), Superseded>>,
    lease: WriterLease,
}

impl Harness {
    /// A fresh `chain`, indexed; `None` when the suite skips.
    async fn open(chain: u64) -> Option<Self> {
        let Some(url) = database_url() else {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        };
        if !anvil_available() {
            eprintln!("skipping: anvil not on PATH");
            return None;
        }
        let pool = db::connect_and_migrate(&url).await.expect("database");
        ChainStore::new(pool.clone(), chain as i64).forget().await;
        let store = ChainStore::new(pool.clone(), chain as i64);

        let node = Node::spawn(chain);
        let libid = Libid::deploy(&node.provider, node.accounts()[0]).await;
        let lease = store.acquire_writer().await.expect("writer lease");
        let writer = store
            .prepare(&lease, libid.contracts())
            .await
            .expect("prepare");
        let cancel = CancellationToken::new();
        let indexer = tokio::spawn(
            indexer::Indexer::new(
                writer,
                node.rpc(),
                indexer::IndexerConfig {
                    contracts: libid.contracts(),
                    confirmations: 0,
                    source: indexer::LogSource::Poll,
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
            node,
            libid,
            cancel,
            indexer,
            lease,
        })
    }

    /// Stop the loop and release the lease.
    async fn close(self) {
        self.cancel.cancel();
        let _ = self.indexer.await;
        self.lease.release().await.expect("the lease releases");
    }

    /// The answer to one GET on the read API, scoped to this chain.
    async fn get<T: DeserializeOwned + Debug>(&self, path: &str) -> T {
        let reply: Reply<T> = self.pool.get_on(self.chain as i64, path).await;
        reply.answer()
    }

    /// Wait for the indexer to commit every block mined so far.
    async fn settle(&self) {
        let latest = self.node.provider.get_block_number().await.expect("latest");
        ChainWatch::new(&self.pool, &self.store)
            .await
            .cursor_at(latest)
            .await;
    }

    /// The anvil accounts that act in the scenario, after the deployer.
    fn accounts(&self) -> &[Address] {
        &self.node.accounts()[1..]
    }

    /// Every column holding text, bytes or JSON on this chain where `needle`
    /// is found: what a privacy scan reads.
    async fn where_found(&self, needle: &str) -> Vec<String> {
        let columns: Vec<(String, String, String, bool)> =
            sqlx::query_as(include_str!("sql/text_columns.sql"))
                .fetch_all(&self.pool)
                .await
                .expect("columns");
        let mut found = Vec::new();
        // A table or column name cannot be bound, so each column's scan is
        // composed from the catalog's names.
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

/// What a proof of `id` holding `handle` on X reports.
fn x_proof(id: &'static str, handle: &'static str, observed_at: u64) -> Proof {
    Proof {
        handle: Handle(KnownPlatform::X, handle),
        id,
        observed_at,
    }
}

#[tokio::test]
async fn private_binds_stay_private_and_disclosures_stay_shown() {
    let Some(mut h) = Harness::open(CHAIN).await else {
        return;
    };
    let &[alice, bob, dave, payer, ..] = h.accounts() else {
        panic!("anvil has fewer than five accounts");
    };

    let x = KnownPlatform::X;
    let alice_node = x.handle_node("Alice_1").expect("a handle");
    let alice_id = x.id_node(ALICE_ID);

    // 1. A private bind of @Alice_1: nodes only, resolved by hashing here,
    //    with no text to show.
    let private = x_proof(ALICE_ID, "alice_1", 1000);
    h.libid
        .bind(alice, private, Disclose::Nothing, U256::ZERO)
        .await;
    h.settle().await;
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

    // 2. A disclosing bind that names the handle: shown on the address and
    //    found by search.
    let disclosing = x_proof(ALICE_ID, "alice_1", 2000);
    h.libid
        .bind(alice, disclosing, Disclose::Name, U256::ZERO)
        .await;
    h.settle().await;
    let theirs: AddressResolution = h.get(&format!("/v1/resolve/address/{alice}")).await;
    assert_eq!(theirs.identities.len(), 1, "{theirs:?}");
    assert_eq!(theirs.identities[0].handle.as_deref(), Some("alice_1"));
    assert!(theirs.identities[0].published);
    let found: SearchResults = h.get("/v1/search?q=alice").await;
    let hits: Vec<&str> = found.hits.iter().map(|hit| hit.handle.as_str()).collect();
    assert_eq!(hits, ["alice_1"]);

    // 3. Withdrawn: no longer published, the text still shown.
    h.libid.unpublish(alice).await;
    h.settle().await;
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
        h.pool
            .disclosed(h.chain as i64, alice_node)
            .await
            .as_deref(),
        Some("alice_1")
    );
    let history: AddressHistory = h.get(&format!("/v1/history/address/{alice}")).await;
    assert!(
        history.entries.iter().any(|e| e
            .handle
            .as_ref()
            .and_then(|h| h.handle.as_deref())
            == Some("alice_1")),
        "{history:?}"
    );

    // 4. A public deposit to @Bob_2, then Bob binds it privately: the
    //    escrow's reveal maps the handle to his id node. A private deposit names only
    //    a node, and no platform.
    let bob_node = x.handle_node("bob_2").expect("a handle");
    let bob_id = x.id_node(BOB_ID);
    let carol_node = x.handle_node("carol_3").expect("a handle");
    h.libid
        .deposit_to_handle(payer, Handle(x, "Bob_2"), 100)
        .await;
    h.libid.deposit(payer, Handle(x, "carol_3"), 5).await;
    let bobs_proof = x_proof(BOB_ID, "bob_2", 3000);
    h.libid
        .bind(bob, bobs_proof, Disclose::Nothing, U256::ZERO)
        .await;
    // Dave binds privately and is never disclosed.
    let daves_proof = x_proof(DAVE_ID, "dave_4", 4000);
    h.libid
        .bind(dave, daves_proof, Disclose::Nothing, U256::ZERO)
        .await;
    h.settle().await;

    let by_node: HandleResolution =
        h.get(&format!("/v1/resolve/handle-node/{bob_node}")).await;
    assert_eq!(by_node.bindings[0].handle.as_deref(), Some("bob_2"));
    assert_eq!(by_node.bindings[0].id_node, bob_id);
    assert_eq!(by_node.bindings[0].owner, bob);
    let by_id: IdResolution = h.get(&format!("/v1/resolve/id-node/{bob_id}")).await;
    assert_eq!(by_id.bindings[0].handle.as_deref(), Some("bob_2"));
    let carols: HandleAmounts = h.get(&format!("/v1/escrow/node/{carol_node}")).await;
    assert_eq!(carols.handle.platform_id, None, "{carols:?}");
    assert_eq!(carols.handle.handle, None);
    assert_eq!(carols.amounts[0].amount, U256::from(5u64));
    let bobs: HandleAmounts = h.get("/v1/escrow/handle/x/BOB_2").await;
    assert_eq!(bobs.handle.platform_id, Some(x.id()));
    assert_eq!(bobs.handle.handle.as_deref(), Some("bob_2"));
    let payers: AddressHistory = h.get(&format!("/v1/history/address/{payer}")).await;
    assert_eq!(payers.entries.len(), 2, "{payers:?}");

    // 5. The privacy scan: no id anywhere, no undisclosed handle anywhere,
    //    and the disclosed ones only beside their nodes.
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
            ["handle_nodes.handle"],
            "{handle}"
        );
    }
    let bob_history: AddressHistory = h.get(&format!("/v1/history/address/{bob}")).await;
    let shown = format!("{bob_history:?}");
    for secret in [BOB_ID, "alice_1", "carol_3", "dave_4"] {
        assert!(!shown.contains(secret), "{secret}: {shown}");
    }
    h.close().await;
}
