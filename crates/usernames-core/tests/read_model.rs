//! The read model, exercised end to end: decoded events go through the same
//! apply path the indexer uses, and the assertions go through the same axum
//! handlers a caller would hit.
//!
//! Tests skip silently when `DATABASE_URL` is unset — that is the only
//! legitimate reason. Everything past that check panics on failure, so a
//! broken migration cannot masquerade as "no database around".

mod common;

use std::{
    fmt::Debug,
    time::Duration,
};

use chrono::TimeDelta;
use tokio::time::Instant;

use alloy::primitives::{
    Address,
    Bytes,
    B256,
    U256,
};
use axum::http::StatusCode;
use common::{
    apply,
    apply_tx,
    bind,
    disclosed,
    publish,
    retire,
    unpublish,
    Reply,
};
use serde::de::DeserializeOwned;
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
    db::{
        self,
        ChainStore,
    },
    ens,
    events::NamesEvent,
    nodes::KnownPlatform,
};

/// One chain id for every test; arbitrary, only consistency matters.
const CHAIN: i64 = 31337;
/// A second chain in the same store, for the reads that span chains.
const OTHER_CHAIN: i64 = 31338;

static DB_LOCK: Mutex<()> = Mutex::const_new(());

async fn test_store() -> Option<(ChainStore, PgPool, MutexGuard<'static, ()>)> {
    test_store_with(1).await
}

/// The same, with room for more than one connection.
///
/// Not needed by the lease any more — it opens its own session outside the pool
/// precisely so it cannot starve one — but kept for a test that wants
/// concurrent queries of its own.
async fn test_store_with(
    max_connections: u32,
) -> Option<(ChainStore, PgPool, MutexGuard<'static, ()>)> {
    let guard = DB_LOCK.lock().await;
    let url = common::database_url()?;
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(&url)
        .await
        .expect("DATABASE_URL is set but connecting failed");
    db::MIGRATOR.run(&pool).await.expect("migrations failed");
    // Scoped to this suite's chain, like the anvil suite scopes to its own:
    // neither depends on cargo happening to run test binaries sequentially.
    for chain in [CHAIN, OTHER_CHAIN] {
        common::clear_chain(&pool, chain).await;
    }
    Some((ChainStore::new(pool.clone(), CHAIN), pool, guard))
}

fn addr(byte: u8) -> Address {
    Address::from_slice(&[byte; 20])
}

/// A request scoped to this suite's chain: the store is shared with the
/// anvil suite's chain, and an unscoped read would see both. The reads that
/// span chains have their own test below.
async fn get<T: DeserializeOwned>(pool: &PgPool, path: &str) -> Reply<T> {
    common::get_on(pool, CHAIN, path).await
}

/// The one binding a resolution carries in these single-chain scenarios.
fn only<T: Debug>(bindings: &[T]) -> &T {
    match bindings {
        [one] => one,
        many => panic!("one binding expected: {many:?}"),
    }
}

/// The id-node route for an id, hashed here as a client would.
fn id_route(platform: KnownPlatform, id: &str) -> String {
    format!("/v1/resolve/id-node/{}", platform.id_node(id).unwrap())
}

#[tokio::test]
async fn bind_resolves_all_three_directions() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let alice = addr(0xA1);
    apply_tx(&store, 1, bind(alice, x, "111", "alice_1", 1000, true)).await;

    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/alice_1").await.answer();
    let binding = only(&resolved.bindings);
    assert_eq!(binding.chain_id, CHAIN);
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.id_node, x.id_node("111").unwrap());
    assert_eq!(binding.handle.as_deref(), Some("alice_1"));
    assert_eq!(binding.ceremony_version, 1);

    // The path is hashed the way the circuit hashed it: case folds.
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/Alice_1").await.answer();
    assert_eq!(resolved.handle_node, x.handle_node("alice_1").unwrap());
    assert_eq!(resolved.platform, Some(KnownPlatform::X));

    let resolved: IdResolution = get(&pool, &id_route(x, "111")).await.answer();
    let binding = only(&resolved.bindings);
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.handle.as_deref(), Some("alice_1"));
    assert!(binding.published);

    let resolved: AddressResolution = get(&pool, &format!("/v1/resolve/address/{alice}"))
        .await
        .answer();
    let identity = only(&resolved.identities);
    assert_eq!(identity.handle.as_deref(), Some("alice_1"));
    assert!(identity.published);
    assert!(identity.resolves);
    drop(guard);
}

#[tokio::test]
async fn rename_retires_the_previous_handle() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let alice = addr(0xA1);
    apply_tx(&store, 1, bind(alice, x, "111", "alice_1", 1000, true)).await;
    // The rename's bind retires alice_1 and withdraws its name first.
    let mut rename = vec![retire(x, "alice_1", alice), unpublish(alice)];
    rename.extend(bind(alice, x, "111", "alice_2", 2000, true));
    apply_tx(&store, 2, rename).await;

    // The code is the machine-readable half: a UI renders "retired"
    // differently from "never claimed" without parsing prose.
    let refusal = get::<HandleResolution>(&pool, "/v1/resolve/handle/x/alice_1")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleRetired));

    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/alice_2").await.answer();
    assert_eq!(only(&resolved.bindings).owner, alice);

    let resolved: IdResolution = get(&pool, &id_route(x, "111")).await.answer();
    assert_eq!(only(&resolved.bindings).handle.as_deref(), Some("alice_2"));

    // The retired node keeps its watermark, mirroring the contract's
    // stale-proof gate.
    let watermark: i64 = sqlx::query_scalar(
        "SELECT observed_at FROM names.handles WHERE chain_id = $1 AND handle_node = $2",
    )
    .bind(CHAIN)
    .bind(x.handle_node("alice_1").unwrap())
    .fetch_one(&pool)
    .await
    .expect("watermark row");
    assert_eq!(watermark, 1000);
    drop(guard);
}

#[tokio::test]
async fn takeover_repoints_the_handle_and_orphans_the_old_id() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let (alice, bob) = (addr(0xA1), addr(0xB2));
    // Alice holds the handle, renames her platform account, re-proves under
    // the same handle... then the platform recycles the name to Bob's
    // account, and Bob proves it with a fresher observation.
    apply_tx(&store, 1, bind(alice, x, "111", "popular", 1000, false)).await;
    apply_tx(&store, 2, bind(bob, x, "222", "popular", 2000, false)).await;

    // The handle resolves to Bob now, paired with Bob's id.
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/popular").await.answer();
    let binding = only(&resolved.bindings);
    assert_eq!(binding.owner, bob);
    assert_eq!(binding.id_node, x.id_node("222").unwrap());

    // Alice's id still resolves to her wallet, but the handle is no longer
    // hers to display: the node points at Bob's id.
    let resolved: IdResolution = get(&pool, &id_route(x, "111")).await.answer();
    let binding = only(&resolved.bindings);
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.handle, None);
    drop(guard);
}

/// Another identity proving a handle withdraws its previous holder's name for
/// it first, as `bind` does; the text the chain disclosed shows all the same,
/// under the new holder, whether that bind disclosed it or not.
#[tokio::test]
async fn a_takeover_keeps_the_disclosed_text() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let (tom, uma, val) = (addr(0xC1), addr(0xC2), addr(0xC3));
    let holder_text = |resolved: &HandleResolution| {
        let binding = only(&resolved.bindings);
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
    apply_tx(&store, 1, bind(tom, x, "555", "lee1", 1_000, true)).await;
    let mut takeover = vec![unpublish(tom)];
    takeover.extend(bind(uma, x, "556", "lee1", 2_000, false));
    apply_tx(&store, 2, takeover).await;
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/lee1").await.answer();
    assert_eq!(holder_text(&resolved), (uma, Some("lee1".into())));
    let found: SearchResults = get(&pool, "/v1/search?q=lee1").await.answer();
    assert_eq!(hits_of(&found), [("lee1".to_string(), uma, false)]);
    let umas: AddressResolution = get(&pool, &format!("/v1/resolve/address/{uma}"))
        .await
        .answer();
    assert_eq!(only(&umas.identities).handle.as_deref(), Some("lee1"));
    assert!(!only(&umas.identities).published);

    // (b) Tom discloses @lee2; Val's account takes it, disclosing it again:
    // still one disclosure.
    apply_tx(&store, 3, bind(tom, x, "555", "lee2", 3_000, true)).await;
    let mut takeover = vec![unpublish(tom)];
    takeover.extend(bind(val, x, "557", "lee2", 4_000, true));
    apply_tx(&store, 4, takeover).await;
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/lee2").await.answer();
    assert_eq!(holder_text(&resolved), (val, Some("lee2".into())));
    let found: SearchResults = get(&pool, "/v1/search?q=lee2").await.answer();
    // The exact match ranks first; lee1 follows as a fuzzy neighbor.
    assert_eq!(hits_of(&found)[0], ("lee2".to_string(), val, true));
    let lee2 = x.handle_node("lee2").unwrap();
    assert_eq!(disclosed(&pool, CHAIN, lee2).await.as_deref(), Some("lee2"));
    // Tom's account no longer holds either handle, so it shows neither.
    let toms: AddressResolution = get(&pool, &format!("/v1/resolve/address/{tom}"))
        .await
        .answer();
    assert!(
        toms.identities
            .iter()
            .all(|i| i.handle.is_none() && !i.published),
        "{toms:?}"
    );
    drop(guard);
}

/// `publish` over a name overwrites it and logs no `HandleUnpublished`; the
/// old name's text still shows, after its handle passes, privately, to
/// someone else too.
#[tokio::test]
async fn publishing_another_name_keeps_the_one_it_replaces_disclosed() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let (alice, bob) = (addr(0xD1), addr(0xD2));
    apply_tx(&store, 1, bind(alice, x, "71", "kim", 1_000, true)).await;
    apply_tx(&store, 2, bind(alice, x, "72", "lee", 2_000, false)).await;
    apply(&store, 3, publish(alice, x, "lee")).await;
    let kim = x.handle_node("kim").unwrap();
    assert_eq!(disclosed(&pool, CHAIN, kim).await.as_deref(), Some("kim"));

    // Bob's account takes kim privately: Alice's name is lee, so the bind
    // withdraws nothing on chain.
    apply_tx(&store, 4, bind(bob, x, "71", "kim", 3_000, false)).await;
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/kim").await.answer();
    let binding = only(&resolved.bindings);
    assert_eq!(
        (binding.owner, binding.handle.as_deref()),
        (bob, Some("kim"))
    );
    let found: SearchResults = get(&pool, "/v1/search?q=kim").await.answer();
    assert_eq!(handles_of(&found), ["kim"]);
    assert_eq!(only(&found.hits).owner, bob);
    let bobs: AddressResolution = get(&pool, &format!("/v1/resolve/address/{bob}"))
        .await
        .answer();
    assert_eq!(only(&bobs.identities).handle.as_deref(), Some("kim"));
    let found: SearchResults = get(&pool, "/v1/search?q=lee").await.answer();
    assert_eq!(handles_of(&found), ["lee"]);
    drop(guard);
}

/// `HandleUnpublished` clears the displayed name; the text it disclosed
/// still shows everywhere, histories included.
#[tokio::test]
async fn a_withdrawn_name_is_no_longer_published_but_still_shows() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let alice = addr(0xA1);
    apply_tx(&store, 1, bind(alice, x, "111", "alice_1", 1000, true)).await;
    // A rename retires the old handle and withdraws the name it published,
    // before the new, private bind.
    let mut rename = vec![retire(x, "alice_1", alice), unpublish(alice)];
    rename.extend(bind(alice, x, "111", "alice_2", 2000, false));
    apply_tx(&store, 2, rename).await;

    let address = format!("/v1/resolve/address/{alice}");
    let resolved: AddressResolution = get(&pool, &address).await.answer();
    let identity = only(&resolved.identities);
    assert!(!identity.published);
    assert_eq!(identity.handle, None, "a private bind shows no text");

    // A later `publish` discloses it.
    apply(&store, 3, publish(alice, x, "alice_2")).await;
    let resolved: AddressResolution = get(&pool, &address).await.answer();
    let identity = only(&resolved.identities);
    assert!(identity.published);
    assert_eq!(identity.handle.as_deref(), Some("alice_2"));

    // HandleUnpublished takes the name down; the text stays.
    apply(&store, 4, unpublish(alice)).await;
    let resolved: AddressResolution = get(&pool, &address).await.answer();
    let identity = only(&resolved.identities);
    assert!(!identity.published);
    assert_eq!(identity.handle.as_deref(), Some("alice_2"));
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/alice_2").await.answer();
    assert_eq!(only(&resolved.bindings).handle.as_deref(), Some("alice_2"));
    let found: SearchResults = get(&pool, "/v1/search?q=alice_2").await.answer();
    assert_eq!(handles_of(&found), ["alice_2"]);
    assert!(!only(&found.hits).published);
    let history: AddressHistory = get(&pool, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    let texts: Vec<&str> = history
        .entries
        .iter()
        .filter_map(|entry| entry.handle.as_ref()?.handle.as_deref())
        .collect();
    assert!(texts.contains(&"alice_1"), "{history:?}");
    assert!(texts.contains(&"alice_2"), "{history:?}");
    drop(guard);
}

/// The platforms of a wallet's identities that carry its name.
fn named_platforms(resolved: &AddressResolution) -> Vec<Option<KnownPlatform>> {
    resolved
        .identities
        .iter()
        .filter(|identity| identity.published)
        .map(|identity| identity.platform)
        .collect()
}

/// The names `names.published` holds for a wallet on this suite's chain.
async fn names_held(pool: &PgPool, owner: Address) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM names.published WHERE chain_id = $1 AND owner = $2",
    )
    .bind(CHAIN)
    .bind(owner)
    .fetch_one(pool)
    .await
    .expect("a name count")
}

/// A holder has one name: a name on GitHub replaces the one on X, and
/// `published` marks only the identity whose handle it is. Publishing the X
/// handle again moves the name back.
#[tokio::test]
async fn a_name_on_another_platform_replaces_the_one_before() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (x, github) = (KnownPlatform::X, KnownPlatform::GitHub);
    let eve = addr(0xE1);
    apply_tx(&store, 1, bind(eve, x, "81", "eve_x", 1_000, true)).await;
    apply_tx(&store, 2, bind(eve, github, "82", "eve_gh", 1_000, true)).await;

    let address = format!("/v1/resolve/address/{eve}");
    let resolved: AddressResolution = get(&pool, &address).await.answer();
    assert_eq!(resolved.identities.len(), 2, "{resolved:?}");
    assert_eq!(named_platforms(&resolved), [Some(github)]);
    assert_eq!(names_held(&pool, eve).await, 1);
    let flags = |found: &SearchResults| -> Vec<(String, bool)> {
        let mut flags: Vec<_> = found
            .hits
            .iter()
            .map(|hit| (hit.handle.clone(), hit.published))
            .collect();
        flags.sort();
        flags
    };
    let found: SearchResults = get(&pool, &format!("/v1/search?owner={eve}"))
        .await
        .answer();
    assert_eq!(
        flags(&found),
        [("eve_gh".to_string(), true), ("eve_x".to_string(), false)]
    );
    let resolved: IdResolution = get(&pool, &id_route(x, "81")).await.answer();
    assert!(!only(&resolved.bindings).published);

    apply(&store, 3, publish(eve, x, "eve_x")).await;
    let resolved: AddressResolution = get(&pool, &address).await.answer();
    assert_eq!(named_platforms(&resolved), [Some(x)]);
    assert_eq!(names_held(&pool, eve).await, 1);
    let found: SearchResults = get(&pool, &format!("/v1/search?owner={eve}"))
        .await
        .answer();
    assert_eq!(
        flags(&found),
        [("eve_gh".to_string(), false), ("eve_x".to_string(), true)]
    );
    drop(guard);
}

/// `HandleUnpublished` names no platform: it clears the holder's name
/// wherever it is, and the other platform's identity shows none either.
#[tokio::test]
async fn a_withdrawal_clears_the_name_whatever_its_platform() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (x, github) = (KnownPlatform::X, KnownPlatform::GitHub);
    let eve = addr(0xE2);
    apply_tx(&store, 1, bind(eve, x, "83", "eve_x", 1_000, true)).await;
    apply_tx(&store, 2, bind(eve, github, "84", "eve_gh", 1_000, true)).await;
    apply(&store, 3, unpublish(eve)).await;

    let resolved: AddressResolution = get(&pool, &format!("/v1/resolve/address/{eve}"))
        .await
        .answer();
    assert_eq!(resolved.identities.len(), 2, "{resolved:?}");
    assert_eq!(named_platforms(&resolved), []);
    assert_eq!(names_held(&pool, eve).await, 0);
    let found: SearchResults = get(&pool, &format!("/v1/search?owner={eve}"))
        .await
        .answer();
    assert_eq!(found.hits.len(), 2, "{found:?}");
    assert!(found.hits.iter().all(|hit| !hit.published), "{found:?}");
    drop(guard);
}

/// The handles a search answered with, in its order.
fn handles_of(results: &SearchResults) -> Vec<&str> {
    results.hits.iter().map(|h| h.handle.as_str()).collect()
}

#[tokio::test]
async fn search_ranks_exact_prefix_substring() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let github = KnownPlatform::GitHub;
    apply_tx(&store, 1, bind(addr(1), x, "1", "ali", 1000, true)).await;
    apply_tx(&store, 2, bind(addr(2), x, "2", "alice_1", 1000, true)).await;
    apply_tx(&store, 3, bind(addr(3), x, "3", "malice", 1000, true)).await;
    apply_tx(&store, 4, bind(addr(4), x, "4", "bob", 1000, true)).await;
    apply_tx(&store, 5, bind(addr(5), github, "5", "alicorn", 1000, true)).await;
    // A private bind is never a hit, whatever it would match.
    apply_tx(&store, 6, bind(addr(6), x, "6", "alina", 1000, false)).await;
    // A name its holder withdrew still is, no longer published.
    apply(&store, 7, unpublish(addr(3))).await;

    let results: SearchResults = get(&pool, "/v1/search?q=ali").await.answer();
    // Exact first, then prefix alphabetically, then substring.
    assert_eq!(
        handles_of(&results),
        ["ali", "alice_1", "alicorn", "malice"]
    );
    assert!(!results.hits[3].published);
    // And the hit carries the right binding, not just the right string.
    let exact = &results.hits[0];
    assert_eq!(exact.owner, addr(1));
    assert_eq!(exact.id_node, x.id_node("1").unwrap());
    assert_eq!(exact.handle_node, x.handle_node("ali").unwrap());
    assert!(exact.published);
    assert_eq!(results.query.as_deref(), Some("ali"));

    // Pages walk the same ranked list; a short page is the last one.
    let page: SearchResults = get(&pool, "/v1/search?q=ali&limit=2&offset=1")
        .await
        .answer();
    assert_eq!((page.limit, page.offset), (2, 1));
    assert_eq!(handles_of(&page), ["alice_1", "alicorn"]);
    let page: SearchResults = get(&pool, "/v1/search?q=ali&limit=2&offset=3")
        .await
        .answer();
    assert_eq!(page.hits.len(), 1, "{page:?}");

    // The platform filter narrows to that keyspace.
    let results: SearchResults = get(&pool, "/v1/search?q=ali&platform=github")
        .await
        .answer();
    assert_eq!(handles_of(&results), ["alicorn"]);
    assert_eq!(results.hits[0].platform, Some(KnownPlatform::GitHub));

    // Search folds the way normalization does: case and a leading @ vanish.
    let results: SearchResults = get(&pool, "/v1/search?q=%40ALI").await.answer();
    assert_eq!(results.hits.len(), 4);

    // A wallet's handles, alone or narrowed by text; the two intersect.
    let results: SearchResults = get(&pool, &format!("/v1/search?owner={}", addr(2)))
        .await
        .answer();
    assert_eq!(handles_of(&results), ["alice_1"]);
    assert_eq!(results.owner, Some(addr(2)));
    assert_eq!(results.query, None);
    let results: SearchResults =
        get(&pool, &format!("/v1/search?q=ali&owner={}", addr(1)))
            .await
            .answer();
    assert_eq!(handles_of(&results), ["ali"]);
    let results: SearchResults =
        get(&pool, &format!("/v1/search?q=bob&owner={}", addr(1)))
            .await
            .answer();
    assert!(results.hits.is_empty(), "{results:?}");

    // Neither text nor wallet is not a question, and a wallet must parse.
    let refusal = get::<SearchResults>(&pool, "/v1/search").await.refusal();
    assert_eq!(
        refusal,
        (StatusCode::BAD_REQUEST, ErrorCode::InvalidArgument)
    );
    let refusal = get::<SearchResults>(&pool, "/v1/search?owner=nobody")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::BAD_REQUEST, ErrorCode::InvalidAddress)
    );

    // A retired handle stops matching. Its fuzzy neighbors still do — that
    // is what the trigram branch is for — but the retired name itself is
    // gone from the results.
    apply_tx(
        &store,
        8,
        vec![retire(x, "alice_1", addr(2)), unpublish(addr(2))],
    )
    .await;
    let results: SearchResults = get(&pool, "/v1/search?q=alice_1").await.answer();
    let handles = handles_of(&results);
    assert!(!handles.contains(&"alice_1"), "{handles:?}");
    assert!(!handles.is_empty(), "fuzzy neighbors should still match");

    // LIKE metacharacters match themselves, not everything.
    let results: SearchResults = get(&pool, "/v1/search?q=%25").await.answer();
    assert!(results.hits.is_empty());

    drop(guard);
}

#[tokio::test]
async fn replay_converges_instead_of_duplicating() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let alice = addr(0xA1);
    let events = bind(alice, x, "111", "alice_1", 1000, true);
    apply_tx(&store, 1, events.clone()).await;
    apply_tx(&store, 1, events).await;

    let journal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.events WHERE chain_id = $1")
            .bind(CHAIN)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(journal, 2);
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/alice_1").await.answer();
    assert_eq!(only(&resolved.bindings).owner, alice);

    let disclosures: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.disclosures WHERE chain_id = $1")
            .bind(CHAIN)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(disclosures, 1);
    drop(guard);
}

#[tokio::test]
async fn unclaimed_names_on_a_configured_platform_are_coded_404s() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    // Something was bound on the platform — the branch the absent-platform
    // test cannot reach, and the one that once shipped a decode 500.
    apply_tx(
        &store,
        1,
        bind(addr(0xA1), x, "111", "alice_1", 1000, false),
    )
    .await;

    let refusal = get::<HandleResolution>(&pool, "/v1/resolve/handle/x/zzz")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleNotBound));

    let refusal = get::<IdResolution>(&pool, &id_route(x, "999999"))
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::IdNotBound));
    drop(guard);
}

#[tokio::test]
async fn api_refuses_to_answer_before_the_first_window() {
    let Some((_store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // No window ever committed: resolution must say "not synced", never an
    // authoritative-looking "not bound".
    let refusal = get::<HandleResolution>(&pool, "/v1/resolve/handle/x/alice_1")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NotSynced)
    );
    let (status, _) = get::<SearchResults>(&pool, "/v1/search?q=ali")
        .await
        .refusal();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // Status still answers — it is how a caller learns the sync state: the
    // chain is simply not among those the store holds.
    let status: Status = common::get(&pool, "/v1/status").await.answer();
    assert!(
        status.chains.iter().all(|c| c.chain_id != CHAIN),
        "{status:?}"
    );
    drop(guard);
}

#[tokio::test]
async fn a_nul_byte_in_a_query_is_refused_not_a_500() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    apply_tx(
        &store,
        1,
        bind(addr(9), KnownPlatform::X, "1", "evil", 1000, true),
    )
    .await;
    // No platform issues a handle holding NUL.
    let refusal = get::<HandleResolution>(&pool, "/v1/resolve/handle/x/evil%00")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::NOT_FOUND, ErrorCode::HandleImpossible)
    );
    let (status, _) = get::<SearchResults>(&pool, "/v1/search?q=evil%00")
        .await
        .refusal();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    drop(guard);
}

#[tokio::test]
async fn admin_events_land_in_ops_metadata_and_ceremony_events_bind_nothing() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    let digest = B256::repeat_byte(0xD1);
    apply(
        &store,
        2,
        NamesEvent::ProofVerifierConfigured {
            verifier: addr(0xEE),
        },
    )
    .await;
    apply(
        &store,
        3,
        NamesEvent::CeremonyBound {
            authorization_digest: digest,
            holder: addr(0xA1),
            platform_id: x.id(),
            client_identifier: Bytes::from_static(b"client-a"),
        },
    )
    .await;
    apply(
        &store,
        4,
        NamesEvent::BindFeePaid {
            authorization_digest: digest,
            receiver: addr(0xFE),
            amount: U256::from(1_000u64),
        },
    )
    .await;

    // The Proof Verifier is the chain's one verification component; the
    // status reports it so an operator need not ask the RPC.
    let status: Status = common::get(&pool, "/v1/status").await.answer();
    let chain = status
        .chains
        .iter()
        .find(|c| c.chain_id == CHAIN)
        .unwrap_or_else(|| panic!("chain {CHAIN} is listed: {status:?}"));
    assert_eq!(chain.proof_verifier, Some(addr(0xEE)));

    // The ceremony's own events are journaled with what an operator asks
    // by — read back here through the histories of the addresses they name —
    // and bind nothing.
    let holder: AddressHistory =
        get(&pool, &format!("/v1/history/address/{}", addr(0xA1)))
            .await
            .answer();
    let [entry] = &holder.entries[..] else {
        panic!("one ceremony: {holder:?}");
    };
    assert_eq!(
        entry.event,
        HistoryEvent::CeremonyBound {
            authorization_digest: digest,
            holder: addr(0xA1),
            platform_id: x.id(),
            client_identifier: Bytes::from_static(b"client-a"),
        }
    );
    let receiver: AddressHistory =
        get(&pool, &format!("/v1/history/address/{}", addr(0xFE)))
            .await
            .answer();
    let [entry] = &receiver.entries[..] else {
        panic!("one fee: {receiver:?}");
    };
    assert_eq!(
        entry.event,
        HistoryEvent::BindFeePaid {
            authorization_digest: digest,
            receiver: addr(0xFE),
            amount: U256::from(1_000u64),
        }
    );
    let bound: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.ids WHERE chain_id = $1")
            .bind(CHAIN)
            .fetch_one(&pool)
            .await
            .expect("ids count");
    assert_eq!(bound, 0, "a ceremony event alone binds nothing");
    drop(guard);
}

#[tokio::test]
async fn unbound_handle_and_impossible_text_name_their_codes() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let x = KnownPlatform::X;
    apply_tx(&store, 1, bind(addr(0xA1), x, "111", "alice_1", 1000, true)).await;

    // A known platform's unbound handle is `handle_not_bound`, whether or
    // not anything was ever bound on it; an id misses as unbound the same way.
    let refusal = get::<HandleResolution>(&pool, "/v1/resolve/handle/x/alice")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleNotBound));
    let refusal = get::<HandleResolution>(&pool, "/v1/resolve/handle/github/alice")
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::HandleNotBound));
    let refusal = get::<IdResolution>(&pool, &id_route(KnownPlatform::GitHub, "111"))
        .await
        .refusal();
    assert_eq!(refusal, (StatusCode::NOT_FOUND, ErrorCode::IdNotBound));
    // A platform this build has no tags for cannot hash text at all.
    let foreign = B256::repeat_byte(7);
    let refusal =
        get::<HandleResolution>(&pool, &format!("/v1/resolve/handle/{foreign}/alice_1"))
            .await
            .refusal();
    assert_eq!(
        refusal,
        (StatusCode::BAD_REQUEST, ErrorCode::InvalidPlatform)
    );

    // Text no circuit hashes (a quote) mirrors the contract's deliberate
    // zero-address answer: "no such handle can exist", a 404 with its own
    // code — not "you asked wrong".
    let refusal = get::<HandleResolution>(&pool, "/v1/resolve/handle/x/a%22b")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::NOT_FOUND, ErrorCode::HandleImpossible)
    );
    drop(guard);
}

/// A wallet is the same address on every chain; its bindings are not. With
/// no chain named, a read answers from every chain the store holds and each
/// result says which one it is from; `?chain=` narrows to one.
#[tokio::test]
async fn reads_span_every_chain_in_the_store_unless_one_is_named() {
    let Some((store, pool, guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let other = ChainStore::new(pool.clone(), OTHER_CHAIN);
    let x = KnownPlatform::X;
    let (alice, bob) = (addr(0xA1), addr(0xB2));
    // The same handle, held by different wallets on the two chains.
    apply_tx(&store, 1, bind(alice, x, "111", "alice_1", 1000, true)).await;
    apply_tx(&other, 1, bind(bob, x, "999", "alice_1", 1000, true)).await;

    // Other suites share the store, so only this suite's chains are read.
    let resolved: HandleResolution = common::get(&pool, "/v1/resolve/handle/x/alice_1")
        .await
        .answer();
    let ours: Vec<(i64, Address)> = resolved
        .bindings
        .iter()
        .filter(|b| [CHAIN, OTHER_CHAIN].contains(&b.chain_id))
        .map(|b| (b.chain_id, b.owner))
        .collect();
    assert_eq!(ours, [(CHAIN, alice), (OTHER_CHAIN, bob)], "{resolved:?}");

    let resolved: HandleResolution = common::get(
        &pool,
        &format!("/v1/resolve/handle/x/alice_1?chain={OTHER_CHAIN}"),
    )
    .await
    .answer();
    assert_eq!(only(&resolved.bindings).owner, bob);

    // An address is asked without a chain; each identity names its own.
    let resolved: AddressResolution =
        common::get(&pool, &format!("/v1/resolve/address/{bob}"))
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
    let results: SearchResults = common::get(&pool, "/v1/search?q=alice_1&platform=x")
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
    let refusal = common::get::<HandleResolution>(
        &pool,
        "/v1/resolve/handle/x/alice_1?chain=424242",
    )
    .await
    .refusal();
    assert_eq!(
        refusal,
        (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NotSynced)
    );
    let refusal =
        common::get::<HandleResolution>(&pool, "/v1/resolve/handle/x/alice_1?chain=eden")
            .await
            .refusal();
    assert_eq!(refusal, (StatusCode::BAD_REQUEST, ErrorCode::InvalidChain));
    drop(guard);
}

// ─── The replay gate ────────────────────────────────────────────────────────
// `prepare` is the only thing in the process that deletes a chain's rows, and
// it decides to on two conditions nothing else tests. It also carries one
// deliberate exception — the deployment-block cache survives — which until now
// existed as a comment.

const OTHER_CONTRACT: Address = Address::new([0xcd; 20]);

fn a_contract() -> Address {
    Address::new([0xab; 20])
}

/// Bind one handle and note a deployment block, so a wipe has something to
/// take and something to leave.
async fn seed(store: &ChainStore) {
    apply_tx(
        store,
        1,
        bind(addr(1), KnownPlatform::X, "1", "alice", 1, true),
    )
    .await;
    store
        .set_deploy_block(a_contract(), 4321)
        .await
        .expect("deploy block");
}

#[tokio::test]
async fn preparing_an_unchanged_chain_keeps_everything() {
    let Some((store, _pool, guard)) = test_store_with(2).await else {
        return;
    };
    let writer = store.acquire_writer().await.expect("lease");
    store
        .prepare(&writer, a_contract(), None)
        .await
        .expect("first");
    seed(&store).await;

    // Same version, same contract: nothing to replay.
    store
        .prepare(&writer, a_contract(), None)
        .await
        .expect("second");
    assert!(
        store.cursor().await.unwrap().is_some(),
        "the cursor survived"
    );
    assert!(
        store
            .ens_holder(KnownPlatform::X.handle_node("alice").unwrap())
            .await
            .unwrap()
            .is_some(),
        "the binding survived"
    );
    writer.release().await.expect("the lease releases");
    drop(guard);
}

#[tokio::test]
async fn watching_another_contract_clears_the_chain() {
    // The old contract's bindings are not the new contract's bindings, so
    // inheriting them would answer for an account nobody bound here.
    let Some((store, _pool, guard)) = test_store_with(2).await else {
        return;
    };
    let writer = store.acquire_writer().await.expect("lease");
    store
        .prepare(&writer, a_contract(), None)
        .await
        .expect("first");
    seed(&store).await;

    store
        .prepare(&writer, OTHER_CONTRACT, None)
        .await
        .expect("repoint");

    assert!(
        store.cursor().await.unwrap().is_none(),
        "the cursor must be gone so the scan restarts"
    );
    assert!(
        store
            .ens_holder(KnownPlatform::X.handle_node("alice").unwrap())
            .await
            .unwrap()
            .is_none(),
        "the previous contract's binding must not survive"
    );
    writer.release().await.expect("the lease releases");
    drop(guard);
}

#[tokio::test]
async fn the_deployment_block_cache_survives_a_replay() {
    // The one carve-out, and the reason for it: the cache is keyed by
    // contract, and `eth_getCode` history does not change shape with the read
    // model. Losing it would re-run the binary search over the whole chain on
    // every version bump.
    let Some((store, _pool, guard)) = test_store_with(2).await else {
        return;
    };
    let writer = store.acquire_writer().await.expect("lease");
    store
        .prepare(&writer, a_contract(), None)
        .await
        .expect("first");
    seed(&store).await;

    store
        .prepare(&writer, OTHER_CONTRACT, None)
        .await
        .expect("repoint");

    assert_eq!(
        store.deploy_block(a_contract()).await.unwrap(),
        Some(4321),
        "the deployment-block cache is keyed by contract and must outlive the wipe"
    );
    writer.release().await.expect("the lease releases");
    drop(guard);
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
    let Some((store, _pool, guard)) = test_store_with(2).await else {
        return;
    };
    let first = store.acquire_writer().await.expect("first lease");
    first.release().await.expect("release");

    // Bounded, because the failure mode is a block rather than an error: on
    // the old code this waits for the pool's idle timeout, not forever, and an
    // unbounded await would look like a hung test rather than a broken lock.
    let second = tokio::time::timeout(Duration::from_secs(5), store.acquire_writer())
        .await
        .expect("the second lease was still blocked on the first")
        .expect("second lease");
    second.release().await.expect("release");
    drop(guard);
}

/// The report the indexer makes beside the target expires on its own
/// schedule, by the database's clock, and a renewal moves the expiry without
/// moving the target — which is what keeps a long catch-up alive.
#[tokio::test]
async fn a_report_expires_and_a_renewal_revives_it_without_moving_the_target() {
    let Some((store, _pool, guard)) = test_store().await else {
        return;
    };
    assert!(
        store.set_chain_target(5, Duration::from_secs(1)).await,
        "the target write landed"
    );
    // The report expires on the database's clock, a second from now.
    let deadline = Instant::now() + Duration::from_secs(10);
    let position = loop {
        let position = store.index_position().await.expect("position");
        if position.valid_for.is_some_and(|d| d < TimeDelta::zero()) {
            break position;
        }
        assert!(Instant::now() < deadline, "never expired: {position:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(position.target, Some(5));

    store.touch_chain_target(Duration::from_secs(120)).await;
    let position = store.index_position().await.expect("position");
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
    drop(guard);
}

/// A chain's names are its indexer's declaration: a set, replaced whole on
/// each start, owned by one chain across the store, and listed on
/// `/v1/status`.
#[tokio::test]
async fn a_chain_declares_its_names_and_no_two_chains_share_one() {
    let Some((store, pool, guard)) = test_store_with(2).await else {
        return;
    };
    let name = |s: &str| s.parse::<ens::ChainName>().unwrap();
    let writer = store.acquire_writer().await.expect("lease");
    // In the indexer's order: the chain is prepared, then named.
    store
        .prepare(&writer, a_contract(), None)
        .await
        .expect("prepared");
    store
        .set_chain_names(
            &writer,
            &[name("alpha"), name("alpha-testnet"), name("alpha")],
        )
        .await
        .expect("declared");
    assert_eq!(
        store.chain_names().await.unwrap(),
        ["alpha", "alpha-testnet"]
    );

    // Both names resolve to this chain.
    let reader = db::Store::new(pool.clone());
    for declared in ["alpha", "alpha-testnet"] {
        assert_eq!(reader.chain_named(declared).await.unwrap(), Some(CHAIN));
    }

    // Declaring again replaces the set.
    store
        .set_chain_names(&writer, &[name("alpha")])
        .await
        .expect("declared again");
    assert_eq!(store.chain_names().await.unwrap(), ["alpha"]);

    // Another chain cannot take a name this one holds, and writes nothing.
    let other = ChainStore::new(pool.clone(), OTHER_CHAIN);
    let other_writer = other.acquire_writer().await.expect("other lease");
    let refused = other
        .set_chain_names(&other_writer, &[name("beta"), name("alpha")])
        .await;
    assert!(
        matches!(
            refused,
            Err(db::ChainNamesError::Taken { ref name, chain_id })
                if name == "alpha" && chain_id == CHAIN
        ),
        "{refused:?}"
    );
    assert_eq!(other.chain_names().await.unwrap(), Vec::<String>::new());

    // A dropped name resolves to no chain, like one nobody declared.
    assert_eq!(reader.chain_named("alpha").await.unwrap(), Some(CHAIN));
    assert_eq!(reader.chain_named("alpha-testnet").await.unwrap(), None);
    assert_eq!(reader.chain_named("beta").await.unwrap(), None);

    // Supervision sees the declaration.
    let status: Status = get(&pool, "/v1/status").await.answer();
    let row = status
        .chains
        .iter()
        .find(|c| c.chain_id == CHAIN)
        .expect("this chain is listed");
    assert_eq!(row.names, ["alpha"]);

    other_writer.release().await.expect("release");
    writer.release().await.expect("release");
    drop(guard);
}
