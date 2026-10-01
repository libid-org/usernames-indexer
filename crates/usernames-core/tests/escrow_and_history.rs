//! The escrow's books and the histories, end to end: decoded events go
//! through the apply path the indexer uses, and every assertion goes through
//! the axum handlers a caller hits.
//!
//! Skips silently when `DATABASE_URL` is unset; everything past that check
//! panics on failure. It shares the database with the other suites, so its
//! chain and its handles are its own: the read-model suite reads `alice_1`
//! across every chain the store holds.

mod common;

use alloy::primitives::{
    address,
    Address,
    Bytes,
    B256,
    U256,
};
use axum::http::StatusCode;
use common::Reply;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tokio::sync::{
    Mutex,
    MutexGuard,
};
use usernames_core::{
    api::model::{
        AddressBalances,
        AddressHistory,
        EscrowAmount,
        HandleBalances,
        HandleHistory,
        HistoryEvent,
        Role,
        Status,
        Unclaimed,
    },
    db::{
        self,
        ChainStore,
    },
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes::{
        self,
        KnownPlatform,
    },
};

const CHAIN: i64 = 31401;

/// The chain's own coin, as the escrow names it (EIP-7528).
const NATIVE: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
const USDC: Address = Address::repeat_byte(0x05);
const DAI: Address = Address::repeat_byte(0x0D);

static DB_LOCK: Mutex<()> = Mutex::const_new(());

async fn test_store() -> Option<(ChainStore, PgPool, MutexGuard<'static, ()>)> {
    let guard = DB_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("DATABASE_URL is set but connecting failed");
    db::MIGRATOR.run(&pool).await.expect("migrations failed");
    for table in db::PROJECTION_TABLES {
        sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
            .bind(CHAIN)
            .execute(&pool)
            .await
            .expect("cleanup failed");
    }
    sqlx::query("DELETE FROM names.chain_metadata WHERE chain_id = $1")
        .bind(CHAIN)
        .execute(&pool)
        .await
        .expect("metadata cleanup failed");
    Some((ChainStore::new(pool.clone(), CHAIN), pool, guard))
}

fn addr(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn x() -> B256 {
    nodes::Platform::from_key("x").unwrap().id()
}

fn node(handle: &str) -> B256 {
    nodes::handle_node(x(), &nodes::NormalizedHandle::from_chain(handle))
}

/// Apply one transaction's events at block `block`, in log order, committing
/// like a window. The block's time is the block number plus a constant, so
/// later blocks are later in time too.
async fn apply_tx(store: &ChainStore, block: u64, events: Vec<NamesEvent>) {
    let mut window = store.begin_window().await.expect("begin");
    for (log_index, event) in events.into_iter().enumerate() {
        let pos = LogPosition {
            block_number: block,
            log_index: log_index as u64,
            tx_hash: B256::from_slice(&[block as u8; 32]),
            block_time: 1_700_000_000 + block,
        };
        window.apply(&event, &pos).await.expect("apply");
    }
    window.commit(block).await.expect("commit");
}

async fn apply(store: &ChainStore, block: u64, event: NamesEvent) {
    apply_tx(store, block, vec![event]).await;
}

async fn get<T: DeserializeOwned>(store: &ChainStore, path: &str) -> Reply<T> {
    let separator = if path.contains('?') { '&' } else { '?' };
    common::get(store, &format!("{path}{separator}chain={CHAIN}")).await
}

fn bind(holder: Address, id: &str, handle: &str, observed_at: u64) -> NamesEvent {
    NamesEvent::IdentityBound {
        holder,
        id_node: nodes::id_node(x(), id),
        handle_node: node(handle),
        platform_id: x(),
        id: id.into(),
        handle: handle.into(),
        observed_at,
        published: false,
        ceremony_version: 1,
    }
}

fn deposited(
    handle: &str,
    token: Address,
    depositor: Address,
    refund_to: Address,
    round: u64,
    amount: u64,
) -> NamesEvent {
    NamesEvent::Deposited {
        handle_node: node(handle),
        token,
        refund_to,
        depositor,
        platform_id: x(),
        round: U256::from(round),
        amount: U256::from(amount),
    }
}

fn kinds(entries: &[usernames_core::api::model::HistoryEntry]) -> Vec<&'static str> {
    entries
        .iter()
        .map(|entry| match entry.event {
            HistoryEvent::IdentityBound { .. } => "identity_bound",
            HistoryEvent::HandleRetired { .. } => "handle_retired",
            HistoryEvent::HandleUnpublished { .. } => "handle_unpublished",
            HistoryEvent::PlatformConfigured { .. } => "platform_configured",
            HistoryEvent::ProofVerifierConfigured { .. } => "proof_verifier_configured",
            HistoryEvent::CeremonyBound { .. } => "ceremony_bound",
            HistoryEvent::BindFeePaid { .. } => "bind_fee_paid",
            HistoryEvent::Deposited { .. } => "deposited",
            HistoryEvent::Forwarded { .. } => "forwarded",
            HistoryEvent::Claimed { .. } => "claimed",
            HistoryEvent::Refunded { .. } => "refunded",
        })
        .collect()
}

/// (token, amount) pairs, in the order served.
fn amounts(list: &[EscrowAmount]) -> Vec<(Address, u64)> {
    list.iter()
        .map(|a| (a.token, a.amount.to::<u64>()))
        .collect()
}

#[tokio::test]
async fn a_handle_collects_before_its_bind_and_its_holder_claims_after() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (bob, carol, dave, erin, dave_wallet) =
        (addr(0xB0), addr(0xC0), addr(0xD0), addr(0xE0), addr(0xD1));
    let bob_node = node("bob_1");

    // Nobody holds @bob_1: three deposits escrow. Erin pays for Dave, whose
    // address is the one that may refund.
    apply(&store, 1, deposited("bob_1", NATIVE, carol, carol, 0, 100)).await;
    apply(&store, 2, deposited("bob_1", NATIVE, erin, dave, 0, 50)).await;
    apply(&store, 3, deposited("bob_1", USDC, carol, carol, 0, 7)).await;

    // Known by its node alone: no bind has carried the text.
    let held: HandleBalances = get(&store, &format!("/v1/escrow/node/{bob_node}"))
        .await
        .answer();
    // Token by token: USDC's address sorts before the native coin's.
    assert_eq!(amounts(&held.held), [(USDC, 7), (NATIVE, 150)]);
    assert_eq!(held.handle.platform, Some(KnownPlatform::X));
    assert_eq!(held.handle.handle, None);
    assert!(held.held.iter().all(|a| a.holder.is_none()));
    // By text, the request names the handle; the node is the same.
    let held: HandleBalances = get(&store, "/v1/escrow/handle/x/@Bob_1").await.answer();
    assert_eq!(held.handle.handle_node, bob_node);
    assert_eq!(held.handle.handle.as_deref(), Some("bob_1"));
    assert_eq!(amounts(&held.held), [(USDC, 7), (NATIVE, 150)]);

    // The payers see what they may take back; a payer that named somebody
    // else's refund address sees nothing.
    let carols: AddressBalances = get(&store, &format!("/v1/escrow/address/{carol}"))
        .await
        .answer();
    assert_eq!(amounts(&carols.refundable), [(USDC, 7), (NATIVE, 100)]);
    assert!(carols.claimable.is_empty());
    let daves: AddressBalances = get(&store, &format!("/v1/escrow/address/{dave}"))
        .await
        .answer();
    assert_eq!(amounts(&daves.refundable), [(NATIVE, 50)]);
    let erins: AddressBalances = get(&store, &format!("/v1/escrow/address/{erin}"))
        .await
        .answer();
    assert!(erins.refundable.is_empty() && erins.claimable.is_empty());

    // Dave takes his back, paid to another wallet of his.
    apply(
        &store,
        4,
        NamesEvent::Refunded {
            handle_node: bob_node,
            token: NATIVE,
            refund_to: dave,
            recipient: dave_wallet,
            round: U256::ZERO,
            released: U256::from(50u64),
            received: U256::from(50u64),
        },
    )
    .await;
    let daves: AddressBalances = get(&store, &format!("/v1/escrow/address/{dave}"))
        .await
        .answer();
    assert!(daves.refundable.is_empty());

    // Bob binds @bob_1: what is held is his to claim, under the handle's text.
    apply(&store, 5, bind(bob, "222", "bob_1", 5_000)).await;
    let bobs: AddressBalances = get(&store, &format!("/v1/escrow/address/{bob}"))
        .await
        .answer();
    assert_eq!(amounts(&bobs.claimable), [(USDC, 7), (NATIVE, 100)]);
    assert!(bobs
        .claimable
        .iter()
        .all(|a| a.holder == Some(bob) && a.handle.handle.as_deref() == Some("bob_1")));
    // Carol may still refund until he claims: the race is the contract's.
    let carols: AddressBalances = get(&store, &format!("/v1/escrow/address/{carol}"))
        .await
        .answer();
    assert_eq!(amounts(&carols.refundable), [(USDC, 7), (NATIVE, 100)]);
    assert!(carols.refundable.iter().all(|a| a.holder == Some(bob)));

    // Bob claims the native coin: that round closes, and with it Carol's
    // refund of it. The USDC slot is untouched.
    apply(
        &store,
        6,
        NamesEvent::Claimed {
            handle_node: bob_node,
            token: NATIVE,
            claimer: bob,
            recipient: bob,
            round: U256::ZERO,
            released: U256::from(100u64),
            received: U256::from(100u64),
        },
    )
    .await;
    let bobs: AddressBalances = get(&store, &format!("/v1/escrow/address/{bob}"))
        .await
        .answer();
    assert_eq!(amounts(&bobs.claimable), [(USDC, 7)]);
    let carols: AddressBalances = get(&store, &format!("/v1/escrow/address/{carol}"))
        .await
        .answer();
    assert_eq!(amounts(&carols.refundable), [(USDC, 7)]);

    // A held handle is paid straight through: nothing escrows.
    apply(
        &store,
        7,
        NamesEvent::Forwarded {
            handle_node: bob_node,
            token: NATIVE,
            depositor: carol,
            holder: bob,
            platform_id: x(),
            amount: U256::from(5u64),
            received: U256::from(5u64),
        },
    )
    .await;
    let held: HandleBalances = get(&store, "/v1/escrow/handle/x/bob_1").await.answer();
    assert_eq!(amounts(&held.held), [(USDC, 7)]);
    assert_eq!(held.held[0].round, U256::ZERO);

    // The handle's history spans both sides of the bind, newest first.
    let history: HandleHistory = get(&store, "/v1/history/handle/x/bob_1").await.answer();
    assert_eq!(
        kinds(&history.entries),
        [
            "forwarded",
            "claimed",
            "identity_bound",
            "refunded",
            "deposited",
            "deposited",
            "deposited"
        ]
    );
    assert!(history.next.is_none());
    assert!(history
        .entries
        .iter()
        .all(
            |e| e.handle.as_ref().map(|h| h.handle_node) == Some(bob_node)
                && e.handle.as_ref().and_then(|h| h.handle.as_deref()) == Some("bob_1")
                && e.roles.is_empty()
        ));
    assert_eq!(history.entries[0].block_time, 1_700_000_007);

    // Each address's history: what it did and was paid, with its parts.
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{bob}"))
        .await
        .answer();
    assert_eq!(
        kinds(&history.entries),
        ["forwarded", "claimed", "identity_bound"]
    );
    assert_eq!(history.entries[0].roles, [Role::Holder]);
    assert_eq!(history.entries[1].roles, [Role::Claimer, Role::Recipient]);
    // Carol sees Bob's claim take her native deposit, the one she could no
    // longer refund after it; Dave had refunded his before it.
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{carol}"))
        .await
        .answer();
    assert_eq!(
        kinds(&history.entries),
        ["forwarded", "claimed", "deposited", "deposited"]
    );
    assert_eq!(history.entries[0].roles, [Role::Depositor]);
    assert_eq!(history.entries[1].roles, [Role::RefundTo]);
    assert_eq!(history.entries[2].roles, [Role::Depositor, Role::RefundTo]);
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{dave}"))
        .await
        .answer();
    assert_eq!(kinds(&history.entries), ["refunded", "deposited"]);
    assert_eq!(history.entries[0].roles, [Role::RefundTo]);
    assert_eq!(history.entries[1].roles, [Role::RefundTo]);
    let history: AddressHistory =
        get(&store, &format!("/v1/history/address/{dave_wallet}"))
            .await
            .answer();
    assert_eq!(history.entries[0].roles, [Role::Recipient]);
    let HistoryEvent::Refunded { received, .. } = &history.entries[0].event else {
        panic!("{:?}", history.entries[0]);
    };
    assert_eq!(*received, U256::from(50u64));
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{erin}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Depositor]);

    // Only USDC is still waiting anywhere on this chain.
    let unclaimed: Unclaimed = get(&store, "/v1/escrow/unclaimed").await.answer();
    assert_eq!(amounts(&unclaimed.slots), [(USDC, 7)]);
}

#[tokio::test]
async fn the_unclaimed_list_groups_by_token_and_ranks_by_amount() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let payer = addr(0xC0);
    let deposits = [
        ("h1", USDC, 5),
        ("h2", USDC, 900),
        ("h3", DAI, 40),
        ("h4", NATIVE, 3),
        ("h5", USDC, 60),
        ("h6", DAI, 1),
    ];
    for (block, (handle, token, amount)) in deposits.into_iter().enumerate() {
        apply(
            &store,
            block as u64 + 1,
            deposited(handle, token, payer, payer, 0, amount),
        )
        .await;
    }

    // Token by token, the token's largest amount first.
    let all: Unclaimed = get(&store, "/v1/escrow/unclaimed").await.answer();
    let mut expected = vec![
        (USDC, 900),
        (USDC, 60),
        (USDC, 5),
        (DAI, 40),
        (DAI, 1),
        (NATIVE, 3),
    ];
    expected.sort_by_key(|(token, amount)| (*token, std::cmp::Reverse(*amount)));
    assert_eq!(amounts(&all.slots), expected);

    let usdc: Unclaimed = get(&store, &format!("/v1/escrow/unclaimed?token={USDC}"))
        .await
        .answer();
    assert_eq!(usdc.token, Some(USDC));
    assert_eq!(amounts(&usdc.slots), [(USDC, 900), (USDC, 60), (USDC, 5)]);

    // Pages are stable slices of the same order.
    let page: Unclaimed = get(
        &store,
        &format!("/v1/escrow/unclaimed?token={USDC}&limit=2&offset=1"),
    )
    .await
    .answer();
    assert_eq!(amounts(&page.slots), [(USDC, 60), (USDC, 5)]);
    assert_eq!((page.limit, page.offset), (2, 1));

    let github: Unclaimed = get(&store, "/v1/escrow/unclaimed?platform=github")
        .await
        .answer();
    assert!(github.slots.is_empty());
}

#[tokio::test]
async fn a_history_pages_by_cursor_without_gaps_or_repeats() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let payer = addr(0xC0);
    // Two events in one transaction and three alone: five entries, two of
    // them sharing a block.
    apply_tx(
        &store,
        1,
        vec![
            deposited("p1", NATIVE, payer, payer, 0, 1),
            deposited("p2", NATIVE, payer, payer, 0, 2),
        ],
    )
    .await;
    for block in 2..=4 {
        apply(
            &store,
            block,
            deposited(&format!("p{}", block + 1), NATIVE, payer, payer, 0, block),
        )
        .await;
    }

    let mut seen = Vec::new();
    let mut path = format!("/v1/history/address/{payer}?limit=2");
    loop {
        let page: AddressHistory = get(&store, &path).await.answer();
        assert!(page.entries.len() <= 2);
        seen.extend(page.entries.iter().map(|e| (e.block_number, e.log_index)));
        match page.next {
            Some(next) => {
                path = format!("/v1/history/address/{payer}?limit=2&before={next}")
            }
            None => break,
        }
    }
    assert_eq!(seen, [(4, 0), (3, 0), (2, 0), (1, 1), (1, 0)]);

    let refusal = get::<AddressHistory>(
        &store,
        &format!("/v1/history/address/{payer}?before=yesterday"),
    )
    .await
    .refusal();
    assert_eq!(
        refusal,
        (StatusCode::BAD_REQUEST, "invalid_cursor".to_string())
    );
}

#[tokio::test]
async fn a_bind_names_what_it_took_and_from_whom() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, bob, carol, dave) = (addr(0xA1), addr(0xB2), addr(0xC3), addr(0xD4));

    // The platform recycled @alice to another account, which Bob proves.
    apply(&store, 1, bind(alice, "111", "kim", 1_000)).await;
    apply(&store, 2, bind(bob, "222", "kim", 2_000)).await;
    // Carol's account moves to Dave's wallet, handle and all.
    apply(&store, 3, bind(carol, "333", "carol", 3_000)).await;
    apply(&store, 4, bind(dave, "333", "carol", 4_000)).await;

    let history: AddressHistory = get(&store, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert_eq!(
        kinds(&history.entries),
        ["identity_bound", "identity_bound"]
    );
    assert_eq!(history.entries[0].roles, [Role::PreviousHandleHolder]);
    assert_eq!(history.entries[1].roles, [Role::Holder]);
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{carol}"))
        .await
        .answer();
    assert_eq!(
        history.entries[0].roles,
        [Role::PreviousHandleHolder, Role::PreviousIdHolder]
    );
    // The new holder's own bind names only it.
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{bob}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Holder]);
}

#[tokio::test]
async fn a_retired_handle_is_in_the_history_of_the_wallet_that_held_it() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, bob) = (addr(0xA1), addr(0xB2));
    let retire = |handle: &str, holder: Address| NamesEvent::HandleRetired {
        platform_id: x(),
        handle_node: node(handle),
        holder,
    };
    // Alice renames @kim1 to @kim2: her own bind retires her own handle.
    apply(&store, 1, bind(alice, "777", "kim1", 1_000)).await;
    apply_tx(
        &store,
        2,
        vec![retire("kim1", alice), bind(alice, "777", "kim2", 2_000)],
    )
    .await;
    // The account moves to Bob's wallet and renames to @kim3 in one bind:
    // the retirement names Bob, but @kim2 was Alice's.
    apply_tx(
        &store,
        3,
        vec![retire("kim2", bob), bind(bob, "777", "kim3", 3_000)],
    )
    .await;

    let history: AddressHistory = get(&store, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    let entries: Vec<(&str, &[Role], Option<&str>)> = kinds(&history.entries)
        .into_iter()
        .zip(&history.entries)
        .map(|(kind, e)| {
            (
                kind,
                e.roles.as_slice(),
                e.handle.as_ref().and_then(|h| h.handle.as_deref()),
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            (
                "identity_bound",
                &[Role::PreviousIdHolder][..],
                Some("kim3")
            ),
            (
                "handle_retired",
                &[Role::PreviousHandleHolder][..],
                Some("kim2")
            ),
            ("identity_bound", &[Role::Holder][..], Some("kim2")),
            ("handle_retired", &[Role::Holder][..], Some("kim1")),
            ("identity_bound", &[Role::Holder][..], Some("kim1")),
        ]
    );
    // Bob never held @kim2: his history is his own bind.
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{bob}"))
        .await
        .answer();
    assert_eq!(kinds(&history.entries), ["identity_bound"]);
}

#[tokio::test]
async fn a_bind_fee_is_in_the_payers_history_under_the_handle_it_bought() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, app) = (addr(0xA1), addr(0xFE));
    let digest = B256::repeat_byte(0xD1);
    // One bind's transaction: the binding, its ceremony, its fee.
    apply_tx(
        &store,
        1,
        vec![
            bind(alice, "111", "ann_1", 1_000),
            NamesEvent::CeremonyBound {
                authorization_digest: digest,
                holder: alice,
                platform_id: x(),
                client_identifier: Bytes::from_static(b"app"),
            },
            NamesEvent::BindFeePaid {
                authorization_digest: digest,
                receiver: app,
                amount: U256::from(1_000u64),
            },
        ],
    )
    .await;

    let history: AddressHistory = get(&store, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert_eq!(
        kinds(&history.entries),
        ["bind_fee_paid", "ceremony_bound", "identity_bound"]
    );
    assert!(history.entries.iter().all(|e| e.roles == [Role::Holder]
        && e.handle.as_ref().and_then(|h| h.handle.as_deref()) == Some("ann_1")));
    let HistoryEvent::BindFeePaid { amount, .. } = &history.entries[0].event else {
        panic!("{:?}", history.entries[0]);
    };
    assert_eq!(*amount, U256::from(1_000u64));

    let history: AddressHistory = get(&store, &format!("/v1/history/address/{app}"))
        .await
        .answer();
    assert_eq!(kinds(&history.entries), ["bind_fee_paid"]);
    assert_eq!(history.entries[0].roles, [Role::FeeReceiver]);

    let history: HandleHistory = get(&store, "/v1/history/handle/x/ann_1").await.answer();
    assert_eq!(history.entries.len(), 3);
}

#[tokio::test]
async fn an_unpublish_concerns_the_handle_it_withdrew() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let alice = addr(0xA1);
    let mut published = bind(alice, "111", "ann_1", 1_000);
    if let NamesEvent::IdentityBound { published, .. } = &mut published {
        *published = true;
    }
    apply(&store, 1, published).await;
    let unpublish = NamesEvent::HandleUnpublished {
        holder: alice,
        platform_id: x(),
    };
    apply(&store, 2, unpublish.clone()).await;
    // Nothing is published now: the contract accepts the call, and it
    // concerns no handle.
    apply(&store, 3, unpublish).await;

    let history: HandleHistory = get(&store, "/v1/history/handle/x/ann_1").await.answer();
    assert_eq!(
        kinds(&history.entries),
        ["handle_unpublished", "identity_bound"]
    );
    let history: AddressHistory = get(&store, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert_eq!(
        kinds(&history.entries),
        ["handle_unpublished", "handle_unpublished", "identity_bound"]
    );
    assert!(history.entries[0].handle.is_none());
    assert!(history.entries[1].handle.is_some());
}

#[tokio::test]
async fn the_escrow_routes_name_what_they_refuse() {
    let Some((store, _pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // Before the first window: not synced, not empty.
    let refusal = get::<Unclaimed>(&store, "/v1/escrow/unclaimed")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::SERVICE_UNAVAILABLE, "not_synced".to_string())
    );

    apply(
        &store,
        1,
        NamesEvent::PlatformConfigured { platform_id: x() },
    )
    .await;
    let cases = [
        (
            "/v1/escrow/node/0x1234",
            StatusCode::BAD_REQUEST,
            "invalid_node",
        ),
        (
            "/v1/history/node/alice",
            StatusCode::BAD_REQUEST,
            "invalid_node",
        ),
        (
            "/v1/escrow/unclaimed?token=usdc",
            StatusCode::BAD_REQUEST,
            "invalid_address",
        ),
        (
            "/v1/escrow/address/0xnope",
            StatusCode::BAD_REQUEST,
            "invalid_address",
        ),
        (
            "/v1/history/handle/x/no%20spaces",
            StatusCode::NOT_FOUND,
            "handle_impossible",
        ),
        (
            "/v1/escrow/handle/myspace/tom",
            StatusCode::BAD_REQUEST,
            "invalid_platform",
        ),
        (
            "/v1/escrow/unclaimed?limit=ten",
            StatusCode::BAD_REQUEST,
            "invalid_argument",
        ),
        (
            "/v1/escrow/unclaimed?offset=-",
            StatusCode::BAD_REQUEST,
            "invalid_argument",
        ),
        (
            "/v1/history/handle/x/nobody?limit=1.5",
            StatusCode::BAD_REQUEST,
            "invalid_argument",
        ),
    ];
    for (path, status, code) in cases {
        let refusal = get::<serde_json::Value>(&store, path).await.refusal();
        assert_eq!(refusal, (status, code.to_string()), "{path}");
    }

    // Nothing held is an answer, not an error.
    let held: HandleBalances = get(&store, "/v1/escrow/handle/x/nobody").await.answer();
    assert!(held.held.is_empty());
    let history: HandleHistory =
        get(&store, "/v1/history/handle/x/nobody").await.answer();
    assert!(history.entries.is_empty() && history.next.is_none());
}

#[tokio::test]
async fn the_status_names_the_escrow_and_a_new_one_replays_the_chain() {
    let Some((store, pool, _guard)) = test_store().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let registry = addr(0x11);
    let (escrow, other_escrow) = (addr(0x22), addr(0x33));
    let writer = store.acquire_writer().await.expect("lease");
    store
        .prepare(&writer, registry, Some(escrow))
        .await
        .expect("prepare");
    let payer = addr(0xC0);
    apply(&store, 1, deposited("h1", NATIVE, payer, payer, 0, 1)).await;

    let status: Status = common::get(&store, "/v1/status").await.answer();
    let chain = status
        .chains
        .iter()
        .find(|c| c.chain_id == CHAIN)
        .unwrap_or_else(|| panic!("chain {CHAIN} is listed: {status:?}"));
    assert_eq!(chain.escrow, Some(escrow));
    assert_eq!(chain.contract, Some(registry));

    // The same escrow keeps everything; another one clears the chain.
    store
        .prepare(&writer, registry, Some(escrow))
        .await
        .expect("prepare again");
    let rows = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM names.escrow_held WHERE chain_id = $1",
        )
        .bind(CHAIN)
        .fetch_one(&pool)
        .await
        .expect("count")
    };
    assert_eq!(rows(pool.clone()).await, 1);
    store
        .prepare(&writer, registry, Some(other_escrow))
        .await
        .expect("prepare another");
    assert_eq!(rows(pool.clone()).await, 0);
    assert_eq!(store.escrow().await.expect("escrow"), Some(other_escrow));
    store
        .prepare(&writer, registry, None)
        .await
        .expect("prepare none");
    assert_eq!(store.escrow().await.expect("escrow"), None);
    writer.release().await.expect("release");
}
