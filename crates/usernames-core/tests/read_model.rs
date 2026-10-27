//! The read model, exercised end to end: decoded events go through the same
//! apply path the indexer uses, and the assertions go through the same axum
//! handlers a caller would hit.
//!
//! Tests skip silently when `DATABASE_URL` is unset — that is the only
//! legitimate reason. Everything past that check panics on failure, so a
//! broken migration cannot masquerade as "no database around".

mod proxy;

use std::{
    fmt::Debug,
    time::Duration,
};

use chrono::TimeDelta;

use alloy::primitives::{
    Address,
    Bytes,
    B256,
    U256,
};
use axum::http::StatusCode;
use sqlx::{
    postgres::PgPoolOptions,
    PgPool,
};
use tokio::sync::{
    Mutex,
    MutexGuard,
};
use usernames_core::{
    api::model::{
        AddressHistory,
        AddressResolution,
        ErrorCode,
        HandleResolution,
        HistoryEvent,
        IdResolution,
        SearchResults,
        Status,
    },
    chain::Contracts,
    db::{
        self,
        ChainState,
        ChainStore,
        ChainWriter,
        LeaseLost,
        Superseded,
        WindowError,
    },
    ens,
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    database_url,
    Events,
    IdNode,
    LockWaits,
    Reads,
    Seed,
    Writes,
};

/// One chain id for every test; arbitrary, only consistency matters.
const CHAIN: i64 = 31337;
/// A second chain in the same store, for the reads that span chains.
const OTHER_CHAIN: i64 = 31338;

static DB_LOCK: Mutex<()> = Mutex::const_new(());

/// The suite's two chains in the database the suites share: emptied of
/// whatever an earlier run left there, and held under the suite's lock until
/// [`Chains::done`]. Scoped to the suite's own chains, so no suite depends on
/// cargo running test binaries one at a time.
struct Chains {
    /// The suite's chain.
    store: ChainStore,
    /// The database the suites share.
    pool: PgPool,
    lock: MutexGuard<'static, ()>,
}

impl Chains {
    /// The suite's chains, emptied, over a pool of `connections`; `None`
    /// when `DATABASE_URL` is unset.
    async fn open(connections: u32) -> Option<Self> {
        let lock = DB_LOCK.lock().await;
        let url = database_url()?;
        let pool = PgPoolOptions::new()
            .max_connections(connections)
            .connect(&url)
            .await
            .expect("DATABASE_URL is set but connecting failed");
        db::MIGRATOR.run(&pool).await.expect("migrations failed");
        for chain in [CHAIN, OTHER_CHAIN] {
            ChainStore::new(pool.clone(), chain).forget().await;
        }
        Some(Self {
            store: ChainStore::new(pool.clone(), CHAIN),
            pool,
            lock,
        })
    }

    /// The test is done with the database; the suite's next test may take it.
    fn done(self) {
        let Self { lock, .. } = self;
        drop(lock);
    }
}

/// The one item a list holds in these single-chain scenarios.
trait Only<T> {
    /// The list's only item; any other count fails the test.
    fn only(&self) -> &T;
}

impl<T: Debug> Only<T> for [T] {
    fn only(&self) -> &T {
        match self {
            [one] => one,
            many => panic!("one binding expected: {many:?}"),
        }
    }
}

/// The id-node route for an id.
trait IdRoute {
    /// The route for `id`, hashed here as a client would.
    fn id_route(self, id: &str) -> String;
}

impl IdRoute for KnownPlatform {
    fn id_route(self, id: &str) -> String {
        format!("/v1/resolve/id-node/{}", self.id_node(id))
    }
}

/// An address's published names, as the suite compares them.
trait NamedPlatforms {
    /// The platforms of the identities whose names are published.
    fn named_platforms(&self) -> Vec<Option<KnownPlatform>>;
}

impl NamedPlatforms for AddressResolution {
    fn named_platforms(&self) -> Vec<Option<KnownPlatform>> {
        self.identities
            .iter()
            .filter(|identity| identity.published)
            .map(|identity| identity.platform)
            .collect()
    }
}

/// A search's hits, as the suite compares them.
trait Handles {
    /// The handles found, in the order served.
    fn handles(&self) -> Vec<&str>;
}

impl Handles for SearchResults {
    fn handles(&self) -> Vec<&str> {
        self.hits.iter().map(|h| h.handle.as_str()).collect()
    }
}

/// Rows the API does not serve, read straight from the store.
trait StoreRows {
    /// How many names `owner` publishes on the suite's chain.
    async fn names_held(&self, owner: Address) -> i64;

    /// The names `chain` goes by, as its status lists them.
    async fn names_of(&self, chain: i64) -> Vec<String>;
}

impl StoreRows for PgPool {
    async fn names_held(&self, owner: Address) -> i64 {
        sqlx::query_scalar(include_str!("sql/names_held.sql"))
            .bind(CHAIN)
            .bind(owner)
            .fetch_one(self)
            .await
            .expect("a name count")
    }

    async fn names_of(&self, chain: i64) -> Vec<String> {
        db::Store::new(self.clone())
            .chain_statuses()
            .await
            .expect("the statuses")
            .into_iter()
            .find(|row| row.state.chain_id == chain)
            .map(|row| row.names)
            .expect("the chain has a status row")
    }
}

/// A chain with something for a replay to take and something to leave.
trait Seeded {
    /// Bind one handle and note a deployment block.
    async fn seed(&self);
}

impl Seeded for ChainWriter {
    async fn seed(&self) {
        self.apply_tx(
            1,
            NamesEvent::bind(
                Address::repeat_byte(1),
                KnownPlatform::X,
                "1",
                "alice",
                1,
                true,
            ),
        )
        .await;
        self.set_deploy_block(4321).await.expect("deploy block");
    }
}

#[tokio::test]
async fn bind_resolves_all_three_directions() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let alice = Address::repeat_byte(0xA1);
    writer
        .apply_tx(1, NamesEvent::bind(alice, x, "111", "alice_1", 1000, true))
        .await;

    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/alice_1")
        .await
        .answer();
    let binding = resolved.bindings.only();
    assert_eq!(binding.chain_id, CHAIN);
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.id_node, x.id_node("111"));
    assert_eq!(binding.handle.as_deref(), Some("alice_1"));
    assert_eq!(binding.ceremony_version, 1);

    // The path is hashed the way the circuit hashed it: case folds.
    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/Alice_1")
        .await
        .answer();
    assert_eq!(resolved.handle_node, x.handle_node("alice_1").unwrap());
    assert_eq!(resolved.platform, Some(KnownPlatform::X));

    let resolved: IdResolution =
        chains.pool.get_on(CHAIN, &x.id_route("111")).await.answer();
    let binding = resolved.bindings.only();
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.handle.as_deref(), Some("alice_1"));
    assert!(binding.published);

    let resolved: AddressResolution = chains
        .pool
        .get_on(CHAIN, &format!("/v1/resolve/address/{alice}"))
        .await
        .answer();
    let identity = resolved.identities.only();
    assert_eq!(identity.handle.as_deref(), Some("alice_1"));
    assert!(identity.published);
    assert!(identity.resolves);
    chains.done();
}

#[tokio::test]
async fn rename_retires_the_previous_handle() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let alice = Address::repeat_byte(0xA1);
    writer
        .apply_tx(1, NamesEvent::bind(alice, x, "111", "alice_1", 1000, true))
        .await;
    // The rename's bind retires alice_1 and withdraws its name first.
    let mut rename = vec![
        NamesEvent::retire(x, "alice_1", alice),
        NamesEvent::unpublish(alice),
    ];
    rename.extend(NamesEvent::bind(alice, x, "111", "alice_2", 2000, true));
    writer.apply_tx(2, rename).await;

    // The code is the machine-readable half: a UI renders "retired"
    // differently from "never claimed" without parsing prose.
    let refusal = chains
        .pool
        .get_on::<HandleResolution>(CHAIN, "/v1/resolve/handle/x/alice_1")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleRetired));

    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/alice_2")
        .await
        .answer();
    assert_eq!(resolved.bindings.only().owner, alice);

    let resolved: IdResolution =
        chains.pool.get_on(CHAIN, &x.id_route("111")).await.answer();
    assert_eq!(resolved.bindings.only().handle.as_deref(), Some("alice_2"));

    // The retired node keeps its watermark, mirroring the contract's
    // stale-proof gate.
    let watermark: i64 = sqlx::query_scalar(
        "SELECT observed_at FROM names.handles WHERE chain_id = $1 AND handle_node = $2",
    )
    .bind(CHAIN)
    .bind(x.handle_node("alice_1").unwrap())
    .fetch_one(&chains.pool)
    .await
    .expect("watermark row");
    assert_eq!(watermark, 1000);
    chains.done();
}

#[tokio::test]
async fn takeover_repoints_the_handle_and_orphans_the_old_id() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let (alice, bob) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));
    // Alice holds the handle, renames her platform account, re-proves under
    // the same handle... then the platform recycles the name to Bob's
    // account, and Bob proves it with a fresher observation.
    writer
        .apply_tx(1, NamesEvent::bind(alice, x, "111", "popular", 1000, false))
        .await;
    writer
        .apply_tx(2, NamesEvent::bind(bob, x, "222", "popular", 2000, false))
        .await;

    // The handle resolves to Bob now, paired with Bob's id.
    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/popular")
        .await
        .answer();
    let binding = resolved.bindings.only();
    assert_eq!(binding.owner, bob);
    assert_eq!(binding.id_node, x.id_node("222"));

    // Alice's id still resolves to her wallet, but the handle is no longer
    // hers to display: the node points at Bob's id.
    let resolved: IdResolution =
        chains.pool.get_on(CHAIN, &x.id_route("111")).await.answer();
    let binding = resolved.bindings.only();
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.handle, None);
    chains.done();
}

/// Another identity proving a handle withdraws its previous holder's name for
/// it first, as `bind` does; the text the chain disclosed shows all the same,
/// under the new holder, whether that bind disclosed it or not.
#[tokio::test]
async fn a_takeover_keeps_the_disclosed_text() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let (tom, uma, val) = (
        Address::repeat_byte(0xC1),
        Address::repeat_byte(0xC2),
        Address::repeat_byte(0xC3),
    );
    let holder_text = |resolved: &HandleResolution| {
        let binding = resolved.bindings.only();
        (binding.owner, binding.handle.clone())
    };
    let hits_of = |found: &SearchResults| -> Vec<(String, Address, bool)> {
        found
            .hits
            .iter()
            .map(|hit| (hit.handle.clone(), hit.owner, hit.published))
            .collect()
    };

    // (a) Tom discloses @lee1; Uma's account takes it privately.
    writer
        .apply_tx(1, NamesEvent::bind(tom, x, "555", "lee1", 1_000, true))
        .await;
    let mut takeover = vec![NamesEvent::unpublish(tom)];
    takeover.extend(NamesEvent::bind(uma, x, "556", "lee1", 2_000, false));
    writer.apply_tx(2, takeover).await;
    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/lee1")
        .await
        .answer();
    assert_eq!(holder_text(&resolved), (uma, Some("lee1".into())));
    let found: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=lee1")
        .await
        .answer();
    assert_eq!(hits_of(&found), [("lee1".to_string(), uma, false)]);
    let umas: AddressResolution = chains
        .pool
        .get_on(CHAIN, &format!("/v1/resolve/address/{uma}"))
        .await
        .answer();
    assert_eq!(umas.identities.only().handle.as_deref(), Some("lee1"));
    assert!(!umas.identities.only().published);

    // (b) Tom discloses @lee2; Val's account takes it, disclosing it again:
    // still one disclosure.
    writer
        .apply_tx(3, NamesEvent::bind(tom, x, "555", "lee2", 3_000, true))
        .await;
    let mut takeover = vec![NamesEvent::unpublish(tom)];
    takeover.extend(NamesEvent::bind(val, x, "557", "lee2", 4_000, true));
    writer.apply_tx(4, takeover).await;
    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/lee2")
        .await
        .answer();
    assert_eq!(holder_text(&resolved), (val, Some("lee2".into())));
    let found: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=lee2")
        .await
        .answer();
    // The exact match ranks first; lee1 follows as a fuzzy neighbor.
    assert_eq!(hits_of(&found)[0], ("lee2".to_string(), val, true));
    let lee2 = x.handle_node("lee2").unwrap();
    assert_eq!(
        chains.pool.disclosed(CHAIN, lee2).await.as_deref(),
        Some("lee2")
    );
    // Tom's account no longer holds either handle, so it shows neither.
    let toms: AddressResolution = chains
        .pool
        .get_on(CHAIN, &format!("/v1/resolve/address/{tom}"))
        .await
        .answer();
    assert!(
        toms.identities
            .iter()
            .all(|i| i.handle.is_none() && !i.published),
        "{toms:?}"
    );
    chains.done();
}

/// `publish` over a name overwrites it and logs no `HandleUnpublished`; the
/// old name's text still shows, after its handle passes, privately, to
/// someone else too.
#[tokio::test]
async fn publishing_another_name_keeps_the_one_it_replaces_disclosed() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let (alice, bob) = (Address::repeat_byte(0xD1), Address::repeat_byte(0xD2));
    writer
        .apply_tx(1, NamesEvent::bind(alice, x, "71", "kim", 1_000, true))
        .await;
    writer
        .apply_tx(2, NamesEvent::bind(alice, x, "72", "lee", 2_000, false))
        .await;
    writer
        .apply_tx(3, NamesEvent::publish(alice, x, "lee"))
        .await;
    let kim = x.handle_node("kim").unwrap();
    assert_eq!(
        chains.pool.disclosed(CHAIN, kim).await.as_deref(),
        Some("kim")
    );

    // Bob's account takes kim privately: Alice's name is lee, so the bind
    // withdraws nothing on chain.
    writer
        .apply_tx(4, NamesEvent::bind(bob, x, "71", "kim", 3_000, false))
        .await;
    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/kim")
        .await
        .answer();
    let binding = resolved.bindings.only();
    assert_eq!(
        (binding.owner, binding.handle.as_deref()),
        (bob, Some("kim"))
    );
    let found: SearchResults =
        chains.pool.get_on(CHAIN, "/v1/search?q=kim").await.answer();
    assert_eq!(found.handles(), ["kim"]);
    assert_eq!(found.hits.only().owner, bob);
    let bobs: AddressResolution = chains
        .pool
        .get_on(CHAIN, &format!("/v1/resolve/address/{bob}"))
        .await
        .answer();
    assert_eq!(bobs.identities.only().handle.as_deref(), Some("kim"));
    let found: SearchResults =
        chains.pool.get_on(CHAIN, "/v1/search?q=lee").await.answer();
    assert_eq!(found.handles(), ["lee"]);
    chains.done();
}

/// `HandleUnpublished` clears the displayed name; the text it disclosed
/// still shows everywhere, histories included.
#[tokio::test]
async fn a_withdrawn_name_is_no_longer_published_but_still_shows() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let alice = Address::repeat_byte(0xA1);
    writer
        .apply_tx(1, NamesEvent::bind(alice, x, "111", "alice_1", 1000, true))
        .await;
    // A rename retires the old handle and withdraws the name it published,
    // before the new, private bind.
    let mut rename = vec![
        NamesEvent::retire(x, "alice_1", alice),
        NamesEvent::unpublish(alice),
    ];
    rename.extend(NamesEvent::bind(alice, x, "111", "alice_2", 2000, false));
    writer.apply_tx(2, rename).await;

    let address = format!("/v1/resolve/address/{alice}");
    let resolved: AddressResolution = chains.pool.get_on(CHAIN, &address).await.answer();
    let identity = resolved.identities.only();
    assert!(!identity.published);
    assert_eq!(identity.handle, None, "a private bind shows no text");

    // A later `publish` discloses it.
    writer
        .apply_tx(3, NamesEvent::publish(alice, x, "alice_2"))
        .await;
    let resolved: AddressResolution = chains.pool.get_on(CHAIN, &address).await.answer();
    let identity = resolved.identities.only();
    assert!(identity.published);
    assert_eq!(identity.handle.as_deref(), Some("alice_2"));

    // HandleUnpublished takes the name down; the text stays.
    writer.apply(4, NamesEvent::unpublish(alice)).await;
    let resolved: AddressResolution = chains.pool.get_on(CHAIN, &address).await.answer();
    let identity = resolved.identities.only();
    assert!(!identity.published);
    assert_eq!(identity.handle.as_deref(), Some("alice_2"));
    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/alice_2")
        .await
        .answer();
    assert_eq!(resolved.bindings.only().handle.as_deref(), Some("alice_2"));
    let found: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=alice_2")
        .await
        .answer();
    assert_eq!(found.handles(), ["alice_2"]);
    assert!(!found.hits.only().published);
    let history: AddressHistory = chains
        .pool
        .get_on(CHAIN, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    let texts: Vec<&str> = history
        .entries
        .iter()
        .filter_map(|entry| entry.handle.as_ref()?.handle.as_deref())
        .collect();
    assert!(texts.contains(&"alice_1"), "{history:?}");
    assert!(texts.contains(&"alice_2"), "{history:?}");
    chains.done();
}

/// A holder has one name: a name on GitHub replaces the one on X, and
/// `published` marks only the identity whose handle it is. Publishing the X
/// handle again moves the name back.
#[tokio::test]
async fn a_name_on_another_platform_replaces_the_one_before() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let (x, github) = (KnownPlatform::X, KnownPlatform::GitHub);
    let eve = Address::repeat_byte(0xE1);
    writer
        .apply_tx(1, NamesEvent::bind(eve, x, "81", "eve_x", 1_000, true))
        .await;
    writer
        .apply_tx(
            2,
            NamesEvent::bind(eve, github, "82", "eve_gh", 1_000, true),
        )
        .await;

    let address = format!("/v1/resolve/address/{eve}");
    let resolved: AddressResolution = chains.pool.get_on(CHAIN, &address).await.answer();
    assert_eq!(resolved.identities.len(), 2, "{resolved:?}");
    assert_eq!(resolved.named_platforms(), [Some(github)]);
    assert_eq!(chains.pool.names_held(eve).await, 1);
    let flags = |found: &SearchResults| -> Vec<(String, bool)> {
        let mut flags: Vec<_> = found
            .hits
            .iter()
            .map(|hit| (hit.handle.clone(), hit.published))
            .collect();
        flags.sort();
        flags
    };
    let found: SearchResults = chains
        .pool
        .get_on(CHAIN, &format!("/v1/search?owner={eve}"))
        .await
        .answer();
    assert_eq!(
        flags(&found),
        [("eve_gh".to_string(), true), ("eve_x".to_string(), false)]
    );
    let resolved: IdResolution =
        chains.pool.get_on(CHAIN, &x.id_route("81")).await.answer();
    assert!(!resolved.bindings.only().published);

    writer
        .apply_tx(3, NamesEvent::publish(eve, x, "eve_x"))
        .await;
    let resolved: AddressResolution = chains.pool.get_on(CHAIN, &address).await.answer();
    assert_eq!(resolved.named_platforms(), [Some(x)]);
    assert_eq!(chains.pool.names_held(eve).await, 1);
    let found: SearchResults = chains
        .pool
        .get_on(CHAIN, &format!("/v1/search?owner={eve}"))
        .await
        .answer();
    assert_eq!(
        flags(&found),
        [("eve_gh".to_string(), false), ("eve_x".to_string(), true)]
    );
    chains.done();
}

/// `HandleUnpublished` names no platform: it clears the holder's name
/// wherever it is, and the other platform's identity shows none either.
#[tokio::test]
async fn a_withdrawal_clears_the_name_whatever_its_platform() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let (x, github) = (KnownPlatform::X, KnownPlatform::GitHub);
    let eve = Address::repeat_byte(0xE2);
    writer
        .apply_tx(1, NamesEvent::bind(eve, x, "83", "eve_x", 1_000, true))
        .await;
    writer
        .apply_tx(
            2,
            NamesEvent::bind(eve, github, "84", "eve_gh", 1_000, true),
        )
        .await;
    writer.apply(3, NamesEvent::unpublish(eve)).await;

    let resolved: AddressResolution = chains
        .pool
        .get_on(CHAIN, &format!("/v1/resolve/address/{eve}"))
        .await
        .answer();
    assert_eq!(resolved.identities.len(), 2, "{resolved:?}");
    assert_eq!(resolved.named_platforms(), []);
    assert_eq!(chains.pool.names_held(eve).await, 0);
    let found: SearchResults = chains
        .pool
        .get_on(CHAIN, &format!("/v1/search?owner={eve}"))
        .await
        .answer();
    assert_eq!(found.hits.len(), 2, "{found:?}");
    assert!(found.hits.iter().all(|hit| !hit.published), "{found:?}");
    chains.done();
}

/// Every `HandleRevealed` fills one map from node to text, whichever
/// contract logged it: a bind that reveals its handle without naming it, and
/// an escrow `depositToHandle` before anybody holds the handle. Neither sets
/// a name.
#[tokio::test]
async fn every_reveal_fills_one_map_and_sets_no_name() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let (eve, fay) = (Address::repeat_byte(0xE3), Address::repeat_byte(0xE5));
    let mut reveal_only = NamesEvent::bind(eve, x, "91", "eve_r", 1_000, false);
    reveal_only.push(NamesEvent::revealed(x, "Eve_R"));
    writer.apply_tx(1, reveal_only).await;
    writer.apply(2, NamesEvent::revealed(x, "Fay_E")).await;
    writer
        .apply_tx(3, NamesEvent::bind(fay, x, "92", "fay_e", 2_000, false))
        .await;

    for (holder, handle) in [(eve, "eve_r"), (fay, "fay_e")] {
        let node = x.handle_node(handle).unwrap();
        assert_eq!(
            chains.pool.disclosed(CHAIN, node).await.as_deref(),
            Some(handle)
        );
        let resolved: AddressResolution = chains
            .pool
            .get_on(CHAIN, &format!("/v1/resolve/address/{holder}"))
            .await
            .answer();
        let identity = resolved.identities.only();
        assert_eq!(identity.handle.as_deref(), Some(handle));
        assert!(!identity.published, "{resolved:?}");
        assert_eq!(chains.pool.names_held(holder).await, 0);
    }
    let found: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=fay_e")
        .await
        .answer();
    assert_eq!(found.handles(), ["fay_e"]);
    assert_eq!(found.hits.only().owner, fay);
    chains.done();
}

#[tokio::test]
async fn search_ranks_exact_prefix_substring() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let github = KnownPlatform::GitHub;
    writer
        .apply_tx(
            1,
            NamesEvent::bind(Address::repeat_byte(1), x, "1", "ali", 1000, true),
        )
        .await;
    writer
        .apply_tx(
            2,
            NamesEvent::bind(Address::repeat_byte(2), x, "2", "alice_1", 1000, true),
        )
        .await;
    writer
        .apply_tx(
            3,
            NamesEvent::bind(Address::repeat_byte(3), x, "3", "malice", 1000, true),
        )
        .await;
    writer
        .apply_tx(
            4,
            NamesEvent::bind(Address::repeat_byte(4), x, "4", "bob", 1000, true),
        )
        .await;
    writer
        .apply_tx(
            5,
            NamesEvent::bind(Address::repeat_byte(5), github, "5", "alicorn", 1000, true),
        )
        .await;
    // A private bind is never a hit, whatever it would match.
    writer
        .apply_tx(
            6,
            NamesEvent::bind(Address::repeat_byte(6), x, "6", "alina", 1000, false),
        )
        .await;
    // A name its holder withdrew still is, no longer published.
    writer
        .apply(7, NamesEvent::unpublish(Address::repeat_byte(3)))
        .await;

    let results: SearchResults =
        chains.pool.get_on(CHAIN, "/v1/search?q=ali").await.answer();
    // Exact first, then prefix alphabetically, then substring.
    assert_eq!(results.handles(), ["ali", "alice_1", "alicorn", "malice"]);
    assert!(!results.hits[3].published);
    // And the hit carries the right binding, not just the right string.
    let exact = &results.hits[0];
    assert_eq!(exact.owner, Address::repeat_byte(1));
    assert_eq!(exact.id_node, x.id_node("1"));
    assert_eq!(exact.handle_node, x.handle_node("ali").unwrap());
    assert!(exact.published);
    assert_eq!(results.query.as_deref(), Some("ali"));

    // Pages walk the same ranked list; a short page is the last one.
    let page: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=ali&limit=2&offset=1")
        .await
        .answer();
    assert_eq!((page.limit, page.offset), (2, 1));
    assert_eq!(page.handles(), ["alice_1", "alicorn"]);
    let page: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=ali&limit=2&offset=3")
        .await
        .answer();
    assert_eq!(page.hits.len(), 1, "{page:?}");

    // The platform filter narrows to that keyspace.
    let results: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=ali&platform=github")
        .await
        .answer();
    assert_eq!(results.handles(), ["alicorn"]);
    assert_eq!(results.hits[0].platform, Some(KnownPlatform::GitHub));

    // Search folds the way normalization does: case and a leading @ vanish.
    let results: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=%40ALI")
        .await
        .answer();
    assert_eq!(results.hits.len(), 4);

    // A wallet's handles, alone or narrowed by text; the two intersect.
    let results: SearchResults = chains
        .pool
        .get_on(
            CHAIN,
            &format!("/v1/search?owner={}", Address::repeat_byte(2)),
        )
        .await
        .answer();
    assert_eq!(results.handles(), ["alice_1"]);
    assert_eq!(results.owner, Some(Address::repeat_byte(2)));
    assert_eq!(results.query, None);
    let results: SearchResults = chains
        .pool
        .get_on(
            CHAIN,
            &format!("/v1/search?q=ali&owner={}", Address::repeat_byte(1)),
        )
        .await
        .answer();
    assert_eq!(results.handles(), ["ali"]);
    let results: SearchResults = chains
        .pool
        .get_on(
            CHAIN,
            &format!("/v1/search?q=bob&owner={}", Address::repeat_byte(1)),
        )
        .await
        .answer();
    assert!(results.hits.is_empty(), "{results:?}");

    // Neither text nor wallet is not a question, and a wallet must parse.
    let refusal = chains
        .pool
        .get_on::<SearchResults>(CHAIN, "/v1/search")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::BAD_REQUEST, ErrorCode::InvalidArgument)
    );
    let refusal = chains
        .pool
        .get_on::<SearchResults>(CHAIN, "/v1/search?owner=nobody")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::BAD_REQUEST, ErrorCode::InvalidAddress)
    );

    // A retired handle stops matching. Its fuzzy neighbors still do — that
    // is what the trigram branch is for — but the retired name itself is
    // gone from the results.
    writer
        .apply_tx(
            8,
            vec![
                NamesEvent::retire(x, "alice_1", Address::repeat_byte(2)),
                NamesEvent::unpublish(Address::repeat_byte(2)),
            ],
        )
        .await;
    let results: SearchResults = chains
        .pool
        .get_on(CHAIN, "/v1/search?q=alice_1")
        .await
        .answer();
    let handles = results.handles();
    assert!(!handles.contains(&"alice_1"), "{handles:?}");
    assert!(!handles.is_empty(), "fuzzy neighbors should still match");

    // LIKE metacharacters match themselves, not everything.
    let results: SearchResults =
        chains.pool.get_on(CHAIN, "/v1/search?q=%25").await.answer();
    assert!(results.hits.is_empty());

    chains.done();
}

#[tokio::test]
async fn replay_converges_instead_of_duplicating() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let alice = Address::repeat_byte(0xA1);
    let events = NamesEvent::bind(alice, x, "111", "alice_1", 1000, true);
    writer.apply_tx(1, events.clone()).await;
    writer.apply_tx(1, events).await;

    let journal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.events WHERE chain_id = $1")
            .bind(CHAIN)
            .fetch_one(&chains.pool)
            .await
            .expect("count");
    // The bind, its reveal and its name.
    assert_eq!(journal, 3);
    let resolved: HandleResolution = chains
        .pool
        .get_on(CHAIN, "/v1/resolve/handle/x/alice_1")
        .await
        .answer();
    assert_eq!(resolved.bindings.only().owner, alice);

    let disclosures: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.handle_nodes WHERE chain_id = $1 AND handle IS NOT NULL")
            .bind(CHAIN)
            .fetch_one(&chains.pool)
            .await
            .expect("count");
    assert_eq!(disclosures, 1);
    chains.done();
}

#[tokio::test]
async fn unclaimed_names_on_a_configured_platform_are_coded_404s() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    // Something was bound on the platform — the branch the absent-platform
    // test cannot reach, and the one that once shipped a decode 500.
    writer
        .apply_tx(
            1,
            NamesEvent::bind(
                Address::repeat_byte(0xA1),
                x,
                "111",
                "alice_1",
                1000,
                false,
            ),
        )
        .await;

    let refusal = chains
        .pool
        .get_on::<HandleResolution>(CHAIN, "/v1/resolve/handle/x/zzz")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleNotBound));

    let refusal = chains
        .pool
        .get_on::<IdResolution>(CHAIN, &x.id_route("999999"))
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::IdNotBound));
    chains.done();
}

#[tokio::test]
async fn api_refuses_to_answer_before_the_first_window() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // No window ever committed: resolution must say "not synced", never an
    // authoritative-looking "not bound".
    let refusal = chains
        .pool
        .get_on::<HandleResolution>(CHAIN, "/v1/resolve/handle/x/alice_1")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NotSynced)
    );
    let (status, _) = chains
        .pool
        .get_on::<SearchResults>(CHAIN, "/v1/search?q=ali")
        .await
        .refusal();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // Status still answers — it is how a caller learns the sync state: the
    // chain is simply not among those the store holds.
    let status: Status = chains.pool.get("/v1/status").await.answer();
    assert!(
        status.chains.iter().all(|c| c.chain_id != CHAIN),
        "{status:?}"
    );
    chains.done();
}

#[tokio::test]
async fn a_nul_byte_in_a_query_is_refused_not_a_500() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    writer
        .apply_tx(
            1,
            NamesEvent::bind(
                Address::repeat_byte(9),
                KnownPlatform::X,
                "1",
                "evil",
                1000,
                true,
            ),
        )
        .await;
    // No platform issues a handle holding NUL.
    let refusal = chains
        .pool
        .get_on::<HandleResolution>(CHAIN, "/v1/resolve/handle/x/evil%00")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::NOT_FOUND, ErrorCode::HandleImpossible)
    );
    let (status, _) = chains
        .pool
        .get_on::<SearchResults>(CHAIN, "/v1/search?q=evil%00")
        .await
        .refusal();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    chains.done();
}

#[tokio::test]
async fn admin_events_land_in_ops_metadata_and_ceremony_events_bind_nothing() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    let digest = B256::repeat_byte(0xD1);
    writer
        .apply(
            2,
            NamesEvent::ProofVerifierConfigured {
                verifier: Address::repeat_byte(0xEE),
            },
        )
        .await;
    writer
        .apply(
            3,
            NamesEvent::CeremonyBound {
                authorization_digest: digest,
                holder: Address::repeat_byte(0xA1),
                platform_id: x.id(),
                client_identifier: Bytes::from_static(b"client-a"),
            },
        )
        .await;
    writer
        .apply(
            4,
            NamesEvent::BindFeePaid {
                authorization_digest: digest,
                receiver: Address::repeat_byte(0xFE),
                amount: U256::from(1_000u64),
            },
        )
        .await;

    // The Proof Verifier is the chain's one verification component; the
    // status reports it so an operator need not ask the RPC.
    let status: Status = chains.pool.get("/v1/status").await.answer();
    let chain = status
        .chains
        .iter()
        .find(|c| c.chain_id == CHAIN)
        .unwrap_or_else(|| panic!("chain {CHAIN} is listed: {status:?}"));
    assert_eq!(chain.proof_verifier, Some(Address::repeat_byte(0xEE)));

    // The ceremony's own events are journaled with what an operator asks
    // by — read back here through the histories of the addresses they name —
    // and bind nothing.
    let holder: AddressHistory = chains
        .pool
        .get_on(
            CHAIN,
            &format!("/v1/history/address/{}", Address::repeat_byte(0xA1)),
        )
        .await
        .answer();
    let [entry] = &holder.entries[..] else {
        panic!("one ceremony: {holder:?}");
    };
    assert_eq!(
        entry.event,
        HistoryEvent::CeremonyBound {
            authorization_digest: digest,
            holder: Address::repeat_byte(0xA1),
            platform_id: x.id(),
            client_identifier: Bytes::from_static(b"client-a"),
        }
    );
    let receiver: AddressHistory = chains
        .pool
        .get_on(
            CHAIN,
            &format!("/v1/history/address/{}", Address::repeat_byte(0xFE)),
        )
        .await
        .answer();
    let [entry] = &receiver.entries[..] else {
        panic!("one fee: {receiver:?}");
    };
    assert_eq!(
        entry.event,
        HistoryEvent::BindFeePaid {
            authorization_digest: digest,
            receiver: Address::repeat_byte(0xFE),
            amount: U256::from(1_000u64),
        }
    );
    let bound: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.ids WHERE chain_id = $1")
            .bind(CHAIN)
            .fetch_one(&chains.pool)
            .await
            .expect("ids count");
    assert_eq!(bound, 0, "a ceremony event alone binds nothing");
    chains.done();
}

#[tokio::test]
async fn unbound_handle_and_impossible_text_name_their_codes() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let x = KnownPlatform::X;
    writer
        .apply_tx(
            1,
            NamesEvent::bind(Address::repeat_byte(0xA1), x, "111", "alice_1", 1000, true),
        )
        .await;

    // A known platform's unbound handle is `handle_not_bound`, whether or
    // not anything was ever bound on it; an id misses as unbound the same way.
    let refusal = chains
        .pool
        .get_on::<HandleResolution>(CHAIN, "/v1/resolve/handle/x/alice")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleNotBound));
    let refusal = chains
        .pool
        .get_on::<HandleResolution>(CHAIN, "/v1/resolve/handle/github/alice")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleNotBound));
    let refusal = chains
        .pool
        .get_on::<IdResolution>(CHAIN, &KnownPlatform::GitHub.id_route("111"))
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::IdNotBound));
    // A platform this build has no tags for cannot hash text at all.
    let foreign = B256::repeat_byte(7);
    let refusal = chains
        .pool
        .get_on::<HandleResolution>(
            CHAIN,
            &format!("/v1/resolve/handle/{foreign}/alice_1"),
        )
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::BAD_REQUEST, ErrorCode::InvalidPlatform)
    );

    // Text no circuit hashes (a quote) mirrors the contract's deliberate
    // zero-address answer: "no such handle can exist", a 404 with its own
    // code — not "you asked wrong".
    let refusal = chains
        .pool
        .get_on::<HandleResolution>(CHAIN, "/v1/resolve/handle/x/a%22b")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::NOT_FOUND, ErrorCode::HandleImpossible)
    );
    chains.done();
}

/// A wallet is the same address on every chain; its bindings are not. With
/// no chain named, a read answers from every chain the store holds and each
/// result says which one it is from; `?chain=` narrows to one.
#[tokio::test]
async fn reads_span_every_chain_in_the_store_unless_one_is_named() {
    let Some(chains) = Chains::open(1).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let writer = chains.store.writer().await;
    let other = ChainStore::new(chains.pool.clone(), OTHER_CHAIN)
        .writer()
        .await;
    let x = KnownPlatform::X;
    let (alice, bob) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));
    // The same handle, held by different wallets on the two chains.
    writer
        .apply_tx(1, NamesEvent::bind(alice, x, "111", "alice_1", 1000, true))
        .await;
    other
        .apply_tx(1, NamesEvent::bind(bob, x, "999", "alice_1", 1000, true))
        .await;

    // Other suites share the store, so only this suite's chains are read.
    let resolved: HandleResolution = chains
        .pool
        .get("/v1/resolve/handle/x/alice_1")
        .await
        .answer();
    let ours: Vec<(i64, Address)> = resolved
        .bindings
        .iter()
        .filter(|b| [CHAIN, OTHER_CHAIN].contains(&b.chain_id))
        .map(|b| (b.chain_id, b.owner))
        .collect();
    assert_eq!(ours, [(CHAIN, alice), (OTHER_CHAIN, bob)], "{resolved:?}");

    let resolved: HandleResolution = chains
        .pool
        .get(&format!("/v1/resolve/handle/x/alice_1?chain={OTHER_CHAIN}"))
        .await
        .answer();
    assert_eq!(resolved.bindings.only().owner, bob);

    // An address is asked without a chain; each identity names its own.
    let resolved: AddressResolution = chains
        .pool
        .get(&format!("/v1/resolve/address/{bob}"))
        .await
        .answer();
    assert!(
        resolved
            .identities
            .iter()
            .any(|i| i.chain_id == OTHER_CHAIN),
        "{resolved:?}"
    );
    assert!(
        resolved.identities.iter().all(|i| i.chain_id != CHAIN),
        "{resolved:?}"
    );

    // Search too, and a hit says where it lives.
    let results: SearchResults = chains
        .pool
        .get("/v1/search?q=alice_1&platform=x")
        .await
        .answer();
    let hit_chains: Vec<i64> = results
        .hits
        .iter()
        .filter(|h| h.handle == "alice_1")
        .map(|h| h.chain_id)
        .collect();
    assert!(
        hit_chains.contains(&CHAIN) && hit_chains.contains(&OTHER_CHAIN),
        "{results:?}"
    );

    // A chain nobody indexed is not an error to ask about; it has nothing
    // to answer from, and says so.
    let refusal = chains
        .pool
        .get::<HandleResolution>("/v1/resolve/handle/x/alice_1?chain=424242")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NotSynced)
    );
    let refusal = chains
        .pool
        .get::<HandleResolution>("/v1/resolve/handle/x/alice_1?chain=eden")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::BAD_REQUEST, ErrorCode::InvalidChain));
    chains.done();
}

// ─── The replay gate ────────────────────────────────────────────────────────
// `prepare` is the only thing in the process that deletes a chain's rows, and
// it decides to on two conditions nothing else tests. The registry's
// deployment block survives a replay of the same registry.

const OTHER_CONTRACT: Address = Address::new([0xcd; 20]);
const OTHER_ESCROW: Address = Address::new([0xce; 20]);

fn a_contract() -> Address {
    Address::new([0xab; 20])
}

#[tokio::test]
async fn preparing_an_unchanged_chain_keeps_everything() {
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let lease = chains.store.acquire_writer().await.expect("lease");
    let writer = chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: a_contract(),
                escrow: None,
            },
        )
        .await
        .expect("first");
    writer.seed().await;

    // Same version, same contract: nothing to replay.
    chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: a_contract(),
                escrow: None,
            },
        )
        .await
        .expect("second");
    assert!(
        chains.store.cursor().await.unwrap().is_some(),
        "the cursor survived"
    );
    assert!(
        chains
            .store
            .ens_answer(KnownPlatform::X.handle_node("alice").unwrap())
            .await
            .unwrap()
            .holder
            .is_some(),
        "the binding survived"
    );
    lease.release().await.expect("the lease releases");
    chains.done();
}

#[tokio::test]
async fn watching_another_contract_clears_the_chain() {
    // The old contract's bindings are not the new contract's bindings, so
    // inheriting them would answer for an account nobody bound here.
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let lease = chains.store.acquire_writer().await.expect("lease");
    let writer = chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: a_contract(),
                escrow: None,
            },
        )
        .await
        .expect("first");
    writer.seed().await;
    writer.set_chain_head(9).await;
    writer.set_chain_target(7, Duration::from_secs(60)).await;
    writer.set_window_error(Some("a window failed")).await;
    let verifier = NamesEvent::ProofVerifierConfigured {
        verifier: Address::repeat_byte(9),
    };
    writer.apply(2, verifier).await;

    chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: OTHER_CONTRACT,
                escrow: Some(OTHER_ESCROW),
            },
        )
        .await
        .expect("repoint");

    assert!(
        chains.store.cursor().await.unwrap().is_none(),
        "the cursor must be gone so the scan restarts"
    );
    // Everything the chain's row held about the old contract goes with it.
    assert_eq!(
        chains.store.state().await.unwrap(),
        ChainState {
            chain_id: chains.store.chain_id(),
            indexer_version: Some(db::INDEXER_VERSION.to_string()),
            contract: Some(OTHER_CONTRACT),
            escrow: Some(OTHER_ESCROW),
            ..ChainState::default()
        }
    );
    assert!(
        chains
            .store
            .ens_answer(KnownPlatform::X.handle_node("alice").unwrap())
            .await
            .unwrap()
            .holder
            .is_none(),
        "the previous contract's binding must not survive"
    );
    lease.release().await.expect("the lease releases");
    chains.done();
}

#[tokio::test]
async fn the_deployment_block_survives_a_replay_of_the_same_registry() {
    // A replay restarts from the registry's deployment block; losing it would
    // re-run the binary search over the whole chain. Another registry has
    // another deployment block.
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let lease = chains.store.acquire_writer().await.expect("lease");
    let writer = chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: a_contract(),
                escrow: None,
            },
        )
        .await
        .expect("first");
    writer.seed().await;

    // A newly watched escrow replays the chain from the same registry.
    let escrow = Address::repeat_byte(0xE5);
    chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: a_contract(),
                escrow: Some(escrow),
            },
        )
        .await
        .expect("an escrow added");
    assert!(
        chains.store.cursor().await.unwrap().is_none(),
        "the chain replays"
    );
    assert_eq!(
        chains.store.deploy_block(a_contract()).await.unwrap(),
        Some(4321)
    );

    chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: OTHER_CONTRACT,
                escrow: Some(escrow),
            },
        )
        .await
        .expect("repoint");
    assert_eq!(
        chains.store.deploy_block(OTHER_CONTRACT).await.unwrap(),
        None
    );
    lease.release().await.expect("the lease releases");
    chains.done();
}

/// A lease whose session ends — a database restart, a failover, an idle
/// timeout — has lost its lock, and says so within an interval; while the
/// session lives, it is not lost.
#[tokio::test]
async fn a_lease_whose_session_ends_reports_itself_lost() {
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let mut lease = chains.store.acquire_writer().await.expect("lease");
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            lease.lost(Duration::from_millis(200))
        )
        .await
        .is_err(),
        "a live session's lease is held"
    );
    sqlx::query(include_str!("sql/end_writer_session.sql"))
        .bind(chains.store.chain_id())
        .execute(&chains.pool)
        .await
        .expect("the lease's session ends");
    let lost = tokio::time::timeout(
        Duration::from_secs(5),
        lease.lost(Duration::from_millis(20)),
    )
    .await
    .expect("the lease reports itself lost");
    assert!(matches!(lost, LeaseLost::Ended(_)), "{lost:?}");
    chains.done();
}

/// A lease whose session stops answering — a half-open socket, a hung
/// server — cannot be told from one whose session ended, and is reported
/// lost within two intervals instead of waiting on the socket.
#[tokio::test]
async fn a_lease_whose_session_goes_silent_reports_itself_lost() {
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let database = database_url().expect("the database is set");
    let proxy = proxy::Proxy::start(&database).await;
    let through = PgPool::connect(proxy.url())
        .await
        .expect("the proxy relays");
    let mut lease = ChainStore::new(through, CHAIN)
        .acquire_writer()
        .await
        .expect("lease");
    proxy.silence();
    let lost = tokio::time::timeout(
        Duration::from_secs(5),
        lease.lost(Duration::from_millis(50)),
    )
    .await
    .expect("the lease reports itself lost");
    assert!(matches!(lost, LeaseLost::Silent(_)), "{lost:?}");
    chains.done();
}

/// The lease must give the lock back, not merely stop being referenced.
///
/// It used to hold `pg_try_advisory_lock` on a POOLED connection, so dropping
/// it returned a live session — lock still held — to the idle pool, where it
/// sat for the idle timeout. A second indexer, or the next test, then blocked
/// on a lock nobody was using. Two sequential leases on one chain is the
/// smallest thing that would have caught it.
#[tokio::test]
async fn a_released_lease_frees_the_chain_for_the_next_holder() {
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let first = chains.store.acquire_writer().await.expect("first lease");
    first.release().await.expect("release");

    // Bounded, because the failure mode is a block rather than an error: on
    // the old code this waits for the pool's idle timeout, not forever, and an
    // unbounded await would look like a hung test rather than a broken lock.
    let second =
        tokio::time::timeout(Duration::from_secs(5), chains.store.acquire_writer())
            .await
            .expect("the second lease was still blocked on the first")
            .expect("second lease");
    second.release().await.expect("release");
    chains.done();
}

/// A writer whose chain was prepared again since (its lease lost, another
/// instance started) opens no window and records nothing, so it cannot move
/// the chain under the writer that took it.
#[tokio::test]
async fn a_writer_whose_chain_was_prepared_again_writes_nothing() {
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let stale = chains.store.writer().await;
    stale.seed().await;
    let before = chains.store.state().await.unwrap();
    let current = chains.store.writer().await;

    let refused = stale.begin_window().await.err();
    assert!(
        matches!(
            refused,
            Some(WindowError::Superseded(Superseded { chain_id })) if chain_id == CHAIN
        ),
        "{refused:?}"
    );
    assert!(
        !stale.set_chain_target(9, Duration::from_secs(60)).await,
        "the target lands nowhere"
    );
    stale.touch_chain_target(Duration::from_secs(60)).await;
    stale.set_chain_head(9).await;
    stale.set_window_error(Some("a stale window failed")).await;
    stale
        .set_deploy_block(9)
        .await
        .expect("a write that lands nowhere");
    assert_eq!(chains.store.state().await.unwrap(), before);

    // The writer that prepared the chain last writes as before.
    current
        .apply(2, NamesEvent::unpublish(Address::repeat_byte(1)))
        .await;
    assert_eq!(chains.store.cursor().await.unwrap(), Some(2));
    chains.done();
}

/// A chain whose row was deleted and prepared again names a writer no
/// earlier writer could hold, so one from before the delete stays shut out.
#[tokio::test]
async fn a_writer_from_before_its_chain_row_was_deleted_writes_nothing() {
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let stale = chains.store.writer().await;
    chains.store.forget().await;
    chains.store.writer().await;

    let refused = stale.begin_window().await.err();
    assert!(
        matches!(
            refused,
            Some(WindowError::Superseded(Superseded { chain_id })) if chain_id == CHAIN
        ),
        "{refused:?}"
    );
    chains.done();
}

/// A replay waits for a window the writer before it still has open, then
/// clears what that window committed: it starts from nothing even when the
/// instance it replaced was mid-window.
#[tokio::test]
async fn a_replay_waits_for_the_window_in_flight_and_clears_it() {
    let Some(chains) = Chains::open(3).await else {
        return;
    };
    let stale = chains.store.writer().await;
    let mut window = stale.begin_window().await.expect("a window");
    let events = NamesEvent::bind(
        Address::repeat_byte(1),
        KnownPlatform::X,
        "1",
        "alice",
        1,
        true,
    );
    for (log_index, event) in (0u64..).zip(&events) {
        let at = LogPosition {
            block_number: 1,
            log_index,
            tx_hash: B256::repeat_byte(1),
            block_time: 1_700_000_001,
        };
        window.apply(event, &at).await.expect("apply");
    }

    // The replay runs on sessions of its own name, so the wait below is its
    // wait and no other session's.
    const REPLAY: &str = "replay under test";
    let replaying = ChainStore::new(
        PgPool::connect_with(
            (*chains.pool.connect_options())
                .clone()
                .application_name(REPLAY),
        )
        .await
        .expect("the replay's pool"),
        CHAIN,
    );
    let lease = chains.store.acquire_writer().await.expect("lease");
    let replay = tokio::spawn(async move {
        replaying
            .prepare(
                &lease,
                Contracts {
                    registry: OTHER_CONTRACT,
                    escrow: None,
                },
            )
            .await
            .expect("the replay is prepared");
        lease
    });
    chains.pool.waits_on_a_lock(REPLAY).await;
    window
        .commit(1)
        .await
        .expect("the window in flight commits");
    let lease = replay.await.expect("the replay task");

    assert_eq!(
        chains.store.cursor().await.unwrap(),
        None,
        "the replay starts over"
    );
    assert!(
        chains
            .store
            .ens_answer(KnownPlatform::X.handle_node("alice").unwrap())
            .await
            .unwrap()
            .holder
            .is_none(),
        "what the window committed is cleared"
    );
    lease.release().await.expect("the lease releases");
    chains.done();
}

/// The report the indexer makes beside the target expires on its own
/// schedule, by the database's clock, and a renewal moves the expiry without
/// moving the target — which is what keeps a long catch-up alive.
#[tokio::test]
async fn a_report_expires_and_a_renewal_revives_it_without_moving_the_target() {
    let Some(chains) = Chains::open(1).await else {
        return;
    };
    let writer = chains.store.writer().await;
    assert!(
        writer.set_chain_target(5, Duration::from_secs(1)).await,
        "the target write landed"
    );
    // The report expires on the database's clock, which counts in whole
    // seconds: past what it says is left, and a second more.
    let left = chains
        .store
        .state()
        .await
        .expect("the chain's row")
        .position();
    let left = left
        .valid_for
        .and_then(|d| d.to_std().ok())
        .unwrap_or_default();
    tokio::time::sleep(left + Duration::from_secs(1)).await;
    let position = chains
        .store
        .state()
        .await
        .expect("the chain's row")
        .position();
    assert!(
        position.valid_for.is_some_and(|d| d < TimeDelta::zero()),
        "{position:?}"
    );
    assert_eq!(position.target, Some(5));

    writer.touch_chain_target(Duration::from_secs(120)).await;
    let position = chains
        .store
        .state()
        .await
        .expect("the chain's row")
        .position();
    assert_eq!(
        position.target,
        Some(5),
        "a renewal must not move the target"
    );
    assert!(
        position.valid_for.is_some_and(|d| d > TimeDelta::zero()),
        "{position:?}"
    );
    assert!(position.reported_at.is_some(), "{position:?}");
    chains.done();
}

/// A chain's names are its indexer's declaration: a set, replaced whole on
/// each start, owned by one chain across the store, and listed on
/// `/v1/status`.
#[tokio::test]
async fn a_chain_declares_its_names_and_no_two_chains_share_one() {
    let Some(chains) = Chains::open(2).await else {
        return;
    };
    let name = |s: &str| s.parse::<ens::ChainName>().unwrap();
    let lease = chains.store.acquire_writer().await.expect("lease");
    // In the indexer's order: the chain is prepared, then named.
    chains
        .store
        .prepare(
            &lease,
            Contracts {
                registry: a_contract(),
                escrow: None,
            },
        )
        .await
        .expect("prepared");
    chains
        .store
        .set_chain_names(
            &lease,
            &[name("alpha"), name("alpha-testnet"), name("alpha")],
        )
        .await
        .expect("declared");
    assert_eq!(
        chains.pool.names_of(CHAIN).await,
        ["alpha", "alpha-testnet"]
    );

    // Both names resolve to this chain.
    let reader = db::Store::new(chains.pool.clone());
    for declared in ["alpha", "alpha-testnet"] {
        assert_eq!(reader.chain_named(declared).await.unwrap(), Some(CHAIN));
    }

    // Declaring again replaces the set.
    chains
        .store
        .set_chain_names(&lease, &[name("alpha")])
        .await
        .expect("declared again");
    assert_eq!(chains.pool.names_of(CHAIN).await, ["alpha"]);

    // Another chain cannot take a name this one holds, and writes nothing.
    let other = ChainStore::new(chains.pool.clone(), OTHER_CHAIN);
    let other_lease = other.acquire_writer().await.expect("other lease");
    let refused = other
        .set_chain_names(&other_lease, &[name("beta"), name("alpha")])
        .await;
    assert!(
        matches!(
            refused,
            Err(db::ChainNamesError::Taken { ref name, chain_id })
                if name == "alpha" && chain_id == CHAIN
        ),
        "{refused:?}"
    );

    // A dropped name resolves to no chain, like one nobody declared.
    assert_eq!(reader.chain_named("alpha").await.unwrap(), Some(CHAIN));
    assert_eq!(reader.chain_named("alpha-testnet").await.unwrap(), None);
    assert_eq!(reader.chain_named("beta").await.unwrap(), None);

    // Supervision sees the declaration.
    let status: Status = chains.pool.get_on(CHAIN, "/v1/status").await.answer();
    let row = status
        .chains
        .iter()
        .find(|c| c.chain_id == CHAIN)
        .expect("this chain is listed");
    assert_eq!(row.names, ["alpha"]);

    other_lease.release().await.expect("release");
    lease.release().await.expect("release");
    chains.done();
}
