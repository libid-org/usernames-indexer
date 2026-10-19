//! The escrow's books and the histories, end to end: decoded events go
//! through the apply path the indexer uses, and every assertion goes through
//! the axum handlers a caller hits.
//!
//! Skips silently when `DATABASE_URL` is unset; everything past that check
//! panics on failure. The suite's chain and handles are its own.

mod common;

use std::{
    collections::BTreeMap,
    fmt::Debug,
};

use alloy::primitives::{
    address,
    Address,
    Bytes,
    B256,
    U256,
};
use axum::http::StatusCode;
use common::{
    events,
    Suite,
};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressHistory,
        ErrorCode,
        EscrowAmount,
        HandleAmounts,
        HandleHistory,
        HistoryEntry,
        HistoryEvent,
        Role,
        Status,
        Unclaimed,
    },
    events::{
        EscrowEvent,
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

static SUITE: Mutex<()> = Mutex::const_new(());

fn addr(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn node(handle: &str) -> B256 {
    nodes::handle_node(
        KnownPlatform::X.id(),
        &nodes::NormalizedHandle::from_chain(handle),
    )
}

fn bind(holder: Address, id: &str, handle: &str, observed_at: u64) -> NamesEvent {
    NamesEvent::IdentityBound {
        holder,
        id_node: nodes::id_node(KnownPlatform::X.id(), id),
        handle_node: node(handle),
        platform_id: KnownPlatform::X.id(),
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
    amount: u64,
) -> NamesEvent {
    NamesEvent::Escrow(EscrowEvent::Deposited {
        handle_node: node(handle),
        token,
        refund_to,
        depositor,
        platform_id: KnownPlatform::X.id(),
        round: U256::ZERO,
        amount: U256::from(amount),
    })
}

/// (token, amount) pairs, in the order served.
fn amounts(list: &[EscrowAmount]) -> Vec<(Address, u64)> {
    list.iter()
        .map(|a| (a.token, a.amount.to::<u64>()))
        .collect()
}

/// Per token, the amounts a list serves.
fn by_token(list: &[EscrowAmount]) -> BTreeMap<Address, U256> {
    list.iter().map(|a| (a.token, a.amount)).collect()
}

/// Per token, what a handle's history deposited less what claims and refunds
/// released from it: what the escrow must still hold for the handle. The
/// history is served newest first, so it is replayed from its end.
fn owed(entries: &[HistoryEntry]) -> BTreeMap<Address, U256> {
    let mut owed = BTreeMap::<Address, U256>::new();
    for entry in entries.iter().rev() {
        match &entry.event {
            HistoryEvent::Deposited { token, amount, .. } => {
                *owed.entry(*token).or_default() += *amount;
            }
            HistoryEvent::Claimed {
                token, released, ..
            }
            | HistoryEvent::Refunded {
                token, released, ..
            } => {
                let held = owed.entry(*token).or_default();
                *held = held
                    .checked_sub(*released)
                    .expect("paid out more than was deposited");
            }
            _ => {}
        }
    }
    owed.retain(|_, amount| !amount.is_zero());
    owed
}

#[tokio::test]
async fn a_handle_collects_before_its_bind_and_its_holder_claims_after() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (bob, carol, dave, erin, dave_wallet) =
        (addr(0xB0), addr(0xC0), addr(0xD0), addr(0xE0), addr(0xD1));
    let bob_node = node("bob_1");

    // Nobody holds @bob_1: three deposits escrow. Erin pays for Dave, whose
    // address is the one that may refund.
    suite
        .apply(1, deposited("bob_1", NATIVE, carol, carol, 100))
        .await;
    suite
        .apply(2, deposited("bob_1", NATIVE, erin, dave, 50))
        .await;
    suite
        .apply(3, deposited("bob_1", USDC, carol, carol, 7))
        .await;

    // Known by its node alone: no bind has carried the text. Token by token,
    // descending: the native coin's address sorts above USDC's.
    let held: HandleAmounts = suite
        .get(&format!("/v1/escrow/node/{bob_node}"))
        .await
        .answer();
    assert_eq!(amounts(&held.amounts), [(NATIVE, 150), (USDC, 7)]);
    assert_eq!(held.handle.platform, Some(KnownPlatform::X));
    assert_eq!(held.handle.handle, None);
    assert!(held.amounts.iter().all(|a| a.holder.is_none()));
    // By text, the request names the handle; the node is the same.
    let held: HandleAmounts = suite.get("/v1/escrow/handle/x/@Bob_1").await.answer();
    assert_eq!(held.handle.handle_node, bob_node);
    assert_eq!(held.handle.handle.as_deref(), Some("bob_1"));

    // The payers see what they may take back; a payer that named somebody
    // else's refund address sees nothing.
    let carols: AddressAmounts = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    assert_eq!(amounts(&carols.amounts), [(NATIVE, 100), (USDC, 7)]);
    let daves: AddressAmounts = suite
        .get(&format!("/v1/escrow/refundable/{dave}"))
        .await
        .answer();
    assert_eq!(amounts(&daves.amounts), [(NATIVE, 50)]);
    let erins: AddressAmounts = suite
        .get(&format!("/v1/escrow/refundable/{erin}"))
        .await
        .answer();
    assert!(erins.amounts.is_empty());

    // Dave takes his back, paid to another address of his, through a token
    // that keeps a fee on transfer: the books release what the escrow sent.
    suite
        .apply(
            4,
            NamesEvent::Escrow(EscrowEvent::Refunded {
                handle_node: bob_node,
                token: NATIVE,
                refund_to: dave,
                recipient: dave_wallet,
                round: U256::ZERO,
                released: U256::from(50u64),
                received: U256::from(49u64),
            }),
        )
        .await;
    let daves: AddressAmounts = suite
        .get(&format!("/v1/escrow/refundable/{dave}"))
        .await
        .answer();
    assert!(daves.amounts.is_empty());

    // Bob binds @bob_1: what is held is his to claim, under the handle's text.
    suite.apply(5, bind(bob, "222", "bob_1", 5_000)).await;
    let bobs: AddressAmounts = suite
        .get(&format!("/v1/escrow/claimable/{bob}"))
        .await
        .answer();
    assert_eq!(amounts(&bobs.amounts), [(NATIVE, 100), (USDC, 7)]);
    assert!(bobs
        .amounts
        .iter()
        .all(|a| a.holder == Some(bob) && a.handle.handle.as_deref() == Some("bob_1")));
    // The refund released Dave's 50 from the books, not the 49 he received.
    let history: HandleHistory = suite.get("/v1/history/handle/x/bob_1").await.answer();
    assert_eq!(owed(&history.entries), by_token(&bobs.amounts));
    // Carol may still refund until he claims: the race is the contract's.
    let carols: AddressAmounts = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    assert_eq!(amounts(&carols.amounts), [(NATIVE, 100), (USDC, 7)]);
    assert!(carols.amounts.iter().all(|a| a.holder == Some(bob)));

    // Bob claims the native coin: that round closes, and with it Carol's
    // refund of it. The USDC slot is untouched.
    suite
        .apply(
            6,
            NamesEvent::Escrow(EscrowEvent::Claimed {
                handle_node: bob_node,
                token: NATIVE,
                claimer: bob,
                recipient: bob,
                round: U256::ZERO,
                released: U256::from(100u64),
                received: U256::from(97u64),
            }),
        )
        .await;
    let bobs: AddressAmounts = suite
        .get(&format!("/v1/escrow/claimable/{bob}"))
        .await
        .answer();
    assert_eq!(amounts(&bobs.amounts), [(USDC, 7)]);
    let carols: AddressAmounts = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    assert_eq!(amounts(&carols.amounts), [(USDC, 7)]);

    // A held handle is paid straight through: nothing escrows.
    suite
        .apply(
            7,
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                handle_node: bob_node,
                token: NATIVE,
                depositor: carol,
                holder: bob,
                platform_id: KnownPlatform::X.id(),
                amount: U256::from(5u64),
                received: U256::from(5u64),
            }),
        )
        .await;

    // The books balance: per token, the escrow holds what was deposited less
    // what claims and refunds released.
    let history: HandleHistory = suite.get("/v1/history/handle/x/bob_1").await.answer();
    let held: HandleAmounts = suite.get("/v1/escrow/handle/x/bob_1").await.answer();
    assert_eq!(owed(&history.entries), by_token(&held.amounts));
    assert_eq!(amounts(&held.amounts), [(USDC, 7)]);
    assert_eq!(held.amounts[0].round, U256::ZERO);

    // The handle's history spans both sides of the bind, newest first.
    assert!(
        matches!(
            events(&history.entries)[..],
            [
                HistoryEvent::Forwarded { .. },
                HistoryEvent::Claimed { .. },
                HistoryEvent::IdentityBound { .. },
                HistoryEvent::Refunded { .. },
                HistoryEvent::Deposited { .. },
                HistoryEvent::Deposited { .. },
                HistoryEvent::Deposited { .. },
            ]
        ),
        "{history:?}"
    );
    assert!(history.next.is_none());
    assert!(history.entries.iter().all(|e| {
        let handle = e.handle.as_ref();
        handle.map(|h| h.handle_node) == Some(bob_node)
            && handle.and_then(|h| h.handle.as_deref()) == Some("bob_1")
            && e.roles.is_empty()
    }));
    assert_eq!(history.entries[0].block_time, 1_700_000_007);

    // Each address's history: what it did and was paid, with its parts.
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{bob}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::Forwarded { .. },
            HistoryEvent::Claimed { .. },
            HistoryEvent::IdentityBound { .. },
        ]
    ));
    assert_eq!(history.entries[0].roles, [Role::Holder]);
    assert_eq!(history.entries[1].roles, [Role::Claimer, Role::Recipient]);
    // Carol sees Bob's claim take her native deposit, the one she could no
    // longer refund after it; Dave had refunded his before it.
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{carol}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::Forwarded { .. },
            HistoryEvent::Claimed { .. },
            HistoryEvent::Deposited { .. },
            HistoryEvent::Deposited { .. },
        ]
    ));
    assert_eq!(history.entries[0].roles, [Role::Depositor]);
    assert_eq!(history.entries[1].roles, [Role::RefundTo]);
    assert_eq!(history.entries[2].roles, [Role::Depositor, Role::RefundTo]);
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{dave}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::Refunded { .. },
            HistoryEvent::Deposited { .. }
        ]
    ));
    assert_eq!(history.entries[0].roles, [Role::RefundTo]);
    assert_eq!(history.entries[1].roles, [Role::RefundTo]);
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{dave_wallet}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Recipient]);
    assert!(matches!(
        history.entries[0].event,
        HistoryEvent::Refunded { received, .. } if received == U256::from(49u64)
    ));
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{erin}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Depositor]);

    // Only USDC is still waiting anywhere on this chain.
    let unclaimed: Unclaimed = suite.get("/v1/escrow/unclaimed").await.answer();
    assert_eq!(amounts(&unclaimed.amounts), [(USDC, 7)]);
    suite.done();
}

/// Six slots over three tokens, one deposit each.
async fn six_unclaimed_slots(suite: &Suite) {
    let payer = addr(0xC0);
    let deposits = [
        ("h1", USDC, 5),
        ("h2", USDC, 900),
        ("h3", DAI, 40),
        ("h4", NATIVE, 3),
        ("h5", USDC, 60),
        ("h6", DAI, 1),
    ];
    for (block, (handle, token, amount)) in (1u64..).zip(deposits) {
        suite
            .apply(block, deposited(handle, token, payer, payer, amount))
            .await;
    }
}

#[tokio::test]
async fn the_unclaimed_list_runs_by_token_then_by_amount() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    six_unclaimed_slots(&suite).await;

    // Token by token, descending, each token's largest amount first: the
    // native coin's address sorts above DAI's, DAI's above USDC's.
    let all: Unclaimed = suite.get("/v1/escrow/unclaimed").await.answer();
    assert_eq!(
        amounts(&all.amounts),
        [
            (NATIVE, 3),
            (DAI, 40),
            (DAI, 1),
            (USDC, 900),
            (USDC, 60),
            (USDC, 5),
        ]
    );
    assert!(all.next.is_none());
    suite.done();
}

#[tokio::test]
async fn the_unclaimed_list_narrows_to_one_token() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    six_unclaimed_slots(&suite).await;

    let usdc: Unclaimed = suite
        .get(&format!("/v1/escrow/unclaimed?token={USDC}"))
        .await
        .answer();
    assert_eq!(usdc.token, Some(USDC));
    assert_eq!(amounts(&usdc.amounts), [(USDC, 900), (USDC, 60), (USDC, 5)]);
    suite.done();
}

/// The key each amount sits under: chain, node and token.
fn keys(list: &[EscrowAmount]) -> Vec<(i64, B256, Address)> {
    list.iter()
        .map(|a| (a.chain_id, a.handle.handle_node, a.token))
        .collect()
}

/// Every page of a list one row at a time, following each page's cursor
/// until the last hands out none: the keys in the order served.
async fn every_page<T: DeserializeOwned + Debug>(
    suite: &Suite,
    path: &str,
    page: impl Fn(T) -> (Vec<EscrowAmount>, Option<String>),
) -> Vec<(i64, B256, Address)> {
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let query = match &before {
            Some(cursor) => format!("{path}?limit=1&before={cursor}"),
            None => format!("{path}?limit=1"),
        };
        let (amounts, next) = page(suite.get(&query).await.answer());
        seen.extend(keys(&amounts));
        match next {
            Some(next) => before = Some(next),
            None => return seen,
        }
    }
}

#[tokio::test]
async fn every_escrow_list_pages_by_cursor_without_gaps_or_repeats() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (bob, carol) = (addr(0xB0), addr(0xC0));
    // Two handles Bob will hold and one nobody does, three tokens each.
    for (block, handle) in (1u64..).step_by(3).zip(["p1", "p2", "p3"]) {
        for (offset, token) in (0u64..).zip([NATIVE, USDC, DAI]) {
            suite
                .apply(block + offset, deposited(handle, token, carol, carol, 10))
                .await;
        }
    }
    suite.apply(20, bind(bob, "1", "p1", 1_000)).await;
    suite.apply(21, bind(bob, "2", "p2", 1_000)).await;

    let path = format!("/v1/escrow/claimable/{bob}");
    let whole: AddressAmounts = suite.get(&path).await.answer();
    let paged = every_page(&suite, &path, |p: AddressAmounts| (p.amounts, p.next)).await;
    assert_eq!((paged.len(), paged), (6, keys(&whole.amounts)));

    let path = format!("/v1/escrow/refundable/{carol}");
    let whole: AddressAmounts = suite.get(&path).await.answer();
    let paged = every_page(&suite, &path, |p: AddressAmounts| (p.amounts, p.next)).await;
    assert_eq!((paged.len(), paged), (9, keys(&whole.amounts)));

    let path = format!("/v1/escrow/node/{}", node("p3"));
    let whole: HandleAmounts = suite.get(&path).await.answer();
    let paged = every_page(&suite, &path, |p: HandleAmounts| (p.amounts, p.next)).await;
    assert_eq!((paged.len(), paged), (3, keys(&whole.amounts)));

    let path = "/v1/escrow/unclaimed";
    let whole: Unclaimed = suite.get(path).await.answer();
    let paged = every_page(&suite, path, |p: Unclaimed| (p.amounts, p.next)).await;
    assert_eq!((paged.len(), paged), (9, keys(&whole.amounts)));
    suite.done();
}

#[tokio::test]
async fn a_history_pages_by_cursor_without_gaps_or_repeats() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let payer = addr(0xC0);
    // Two events in one transaction and three alone: five entries, two of
    // them sharing a block.
    suite
        .apply_tx(
            1,
            vec![
                deposited("p1", NATIVE, payer, payer, 1),
                deposited("p2", NATIVE, payer, payer, 2),
            ],
        )
        .await;
    for block in 2..=4 {
        suite
            .apply(
                block,
                deposited(&format!("p{}", block + 1), NATIVE, payer, payer, block),
            )
            .await;
    }

    let mut seen = Vec::new();
    let mut path = format!("/v1/history/address/{payer}?limit=2");
    loop {
        let page: AddressHistory = suite.get(&path).await.answer();
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
    suite.done();
}

#[tokio::test]
async fn a_bind_names_what_it_took_and_from_whom() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, bob, carol, dave) = (addr(0xA1), addr(0xB2), addr(0xC3), addr(0xD4));

    // The platform recycled @kim to another account, which Bob proves.
    suite.apply(1, bind(alice, "111", "kim", 1_000)).await;
    suite.apply(2, bind(bob, "222", "kim", 2_000)).await;
    // Carol's account moves to Dave's address, handle and all.
    suite.apply(3, bind(carol, "333", "carol", 3_000)).await;
    suite.apply(4, bind(dave, "333", "carol", 4_000)).await;

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::IdentityBound { .. },
            HistoryEvent::IdentityBound { .. },
        ]
    ));
    assert_eq!(history.entries[0].roles, [Role::PreviousHandleHolder]);
    assert_eq!(history.entries[1].roles, [Role::Holder]);
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{carol}"))
        .await
        .answer();
    assert_eq!(
        history.entries[0].roles,
        [Role::PreviousHandleHolder, Role::PreviousIdHolder]
    );
    // The new holder's own bind names only it.
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{bob}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Holder]);
    suite.done();
}

#[tokio::test]
async fn a_retired_handle_is_in_the_history_of_the_address_that_held_it() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, bob) = (addr(0xA1), addr(0xB2));
    let retire = |handle: &str, holder: Address| NamesEvent::HandleRetired {
        platform_id: KnownPlatform::X.id(),
        handle_node: node(handle),
        holder,
    };
    // Alice renames @kim1 to @kim2: her own bind retires her own handle.
    suite.apply(1, bind(alice, "777", "kim1", 1_000)).await;
    suite
        .apply_tx(
            2,
            vec![retire("kim1", alice), bind(alice, "777", "kim2", 2_000)],
        )
        .await;
    // The account moves to Bob's address and renames to @kim3 in one bind:
    // the retirement names Bob, but @kim2 was Alice's.
    suite
        .apply_tx(
            3,
            vec![retire("kim2", bob), bind(bob, "777", "kim3", 3_000)],
        )
        .await;

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    let entries: Vec<(&[Role], Option<&str>)> = history
        .entries
        .iter()
        .map(|e| {
            (
                e.roles.as_slice(),
                e.handle.as_ref().and_then(|h| h.handle.as_deref()),
            )
        })
        .collect();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::IdentityBound { .. },
            HistoryEvent::HandleRetired { .. },
            HistoryEvent::IdentityBound { .. },
            HistoryEvent::HandleRetired { .. },
            HistoryEvent::IdentityBound { .. },
        ]
    ));
    assert_eq!(
        entries,
        [
            (&[Role::PreviousIdHolder][..], Some("kim3")),
            (&[Role::PreviousHandleHolder][..], Some("kim2")),
            (&[Role::Holder][..], Some("kim2")),
            (&[Role::Holder][..], Some("kim1")),
            (&[Role::Holder][..], Some("kim1")),
        ]
    );
    // Bob never held @kim2: his history is his own bind.
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{bob}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [HistoryEvent::IdentityBound { .. }]
    ));
    suite.done();
}

#[tokio::test]
async fn a_bind_fee_is_in_the_payers_history_under_the_handle_it_bought() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, app) = (addr(0xA1), addr(0xFE));
    let digest = B256::repeat_byte(0xD1);
    // One bind's transaction: the binding, its ceremony, its fee.
    suite
        .apply_tx(
            1,
            vec![
                bind(alice, "111", "ann_1", 1_000),
                NamesEvent::CeremonyBound {
                    authorization_digest: digest,
                    holder: alice,
                    platform_id: KnownPlatform::X.id(),
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

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::BindFeePaid { amount, .. },
            HistoryEvent::CeremonyBound { .. },
            HistoryEvent::IdentityBound { .. },
        ] if *amount == U256::from(1_000u64)
    ));
    assert!(history.entries.iter().all(|e| e.roles == [Role::Holder]
        && e.handle.as_ref().and_then(|h| h.handle.as_deref()) == Some("ann_1")));

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{app}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [HistoryEvent::BindFeePaid { .. }]
    ));
    assert_eq!(history.entries[0].roles, [Role::FeeReceiver]);

    let history: HandleHistory = suite.get("/v1/history/handle/x/ann_1").await.answer();
    assert_eq!(history.entries.len(), 3);
    suite.done();
}

#[tokio::test]
async fn an_unpublish_concerns_the_handle_it_withdrew() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let alice = addr(0xA1);
    let mut published = bind(alice, "111", "ann_1", 1_000);
    if let NamesEvent::IdentityBound { published, .. } = &mut published {
        *published = true;
    }
    suite.apply(1, published).await;
    suite
        .apply(
            2,
            NamesEvent::HandleUnpublished {
                holder: alice,
                platform_id: KnownPlatform::X.id(),
            },
        )
        .await;

    let history: HandleHistory = suite.get("/v1/history/handle/x/ann_1").await.answer();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::HandleUnpublished { .. },
            HistoryEvent::IdentityBound { .. },
        ]
    ));
    suite.done();
}

#[tokio::test]
async fn an_unpublish_with_nothing_published_concerns_no_handle() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let alice = addr(0xA1);
    suite.apply(1, bind(alice, "111", "ann_1", 1_000)).await;
    suite
        .apply(
            2,
            NamesEvent::HandleUnpublished {
                holder: alice,
                platform_id: KnownPlatform::X.id(),
            },
        )
        .await;

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert!(matches!(
        events(&history.entries)[..],
        [
            HistoryEvent::HandleUnpublished { .. },
            HistoryEvent::IdentityBound { .. },
        ]
    ));
    assert!(history.entries[0].handle.is_none());
    suite.done();
}

#[tokio::test]
async fn an_escrow_list_before_the_first_window_is_not_synced() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let refusal = suite
        .get::<Unclaimed>("/v1/escrow/unclaimed")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NotSynced)
    );
    suite.done();
}

#[tokio::test]
async fn a_handle_nothing_was_sent_to_holds_nothing() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite
        .apply(
            1,
            NamesEvent::PlatformConfigured {
                platform_id: KnownPlatform::X.id(),
            },
        )
        .await;
    let held: HandleAmounts = suite.get("/v1/escrow/handle/x/nobody").await.answer();
    assert!(held.amounts.is_empty() && held.next.is_none());
    let history: HandleHistory = suite.get("/v1/history/handle/x/nobody").await.answer();
    assert!(history.entries.is_empty() && history.next.is_none());
    suite.done();
}

#[tokio::test]
async fn the_status_names_the_escrow_the_chain_was_indexed_from() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (registry, escrow) = (addr(0x11), addr(0x22));
    let writer = suite.store.acquire_writer().await.expect("lease");
    suite
        .store
        .prepare(&writer, registry, Some(escrow))
        .await
        .expect("prepare");

    let status: Status = suite.get("/v1/status").await.answer();
    let chain = status
        .chains
        .iter()
        .find(|c| c.chain_id == CHAIN)
        .unwrap_or_else(|| panic!("chain {CHAIN} is listed: {status:?}"));
    assert_eq!(chain.escrow, Some(escrow));
    assert_eq!(chain.contract, Some(registry));
    writer.release().await.expect("release");
    suite.done();
}

#[tokio::test]
async fn another_escrow_replays_the_chain_and_the_same_one_keeps_it() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let registry = addr(0x11);
    let (escrow, other_escrow) = (addr(0x22), addr(0x33));
    let writer = suite.store.acquire_writer().await.expect("lease");
    suite
        .store
        .prepare(&writer, registry, Some(escrow))
        .await
        .expect("prepare");
    let payer = addr(0xC0);
    suite
        .apply(1, deposited("h1", NATIVE, payer, payer, 1))
        .await;

    suite
        .store
        .prepare(&writer, registry, Some(escrow))
        .await
        .expect("prepare again");
    let kept: Unclaimed = suite.get("/v1/escrow/unclaimed").await.answer();
    assert_eq!(amounts(&kept.amounts), [(NATIVE, 1)]);

    // Another escrow clears the chain, cursor included: nothing is served
    // until the replay commits its first window.
    suite
        .store
        .prepare(&writer, registry, Some(other_escrow))
        .await
        .expect("prepare another");
    let refusal = suite
        .get::<Unclaimed>("/v1/escrow/unclaimed")
        .await
        .refusal();
    assert_eq!(
        refusal,
        (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NotSynced)
    );
    writer.release().await.expect("release");
    suite.done();
}
