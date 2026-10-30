//! The escrow's books and the histories, end to end: decoded events go
//! through the apply path the indexer uses, and every assertion goes through
//! the axum handlers a caller hits.
//!
//! Skips silently when `DATABASE_URL` is unset; everything past that check
//! panics on failure. The suite's chain and handles are its own.

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
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressContributions,
        AddressHistory,
        Contribution,
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
    chain::Contracts,
    db::ApplyError,
    events::{
        EscrowEvent,
        EventKind,
        LogPosition,
        NamesEvent,
    },
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    Entries,
    Events,
    Suite,
    Writes,
};

const CHAIN: i64 = 31401;

/// The chain's own coin, as the escrow names it (EIP-7528).
const NATIVE: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
const USDC: Address = Address::repeat_byte(0x05);
const DAI: Address = Address::repeat_byte(0x0D);

static SUITE: Mutex<()> = Mutex::const_new(());

const X: KnownPlatform = KnownPlatform::X;

fn node(handle: &str) -> B256 {
    X.handle_node(handle).expect("an X handle")
}

/// When the suite's deposits open for refund: the escrow's 30 days past the
/// suite's block times.
const REFUNDABLE_AT: u64 = 1_700_000_000 + 30 * 24 * 60 * 60;

/// The escrow's events, as the suite writes them.
trait EscrowEvents {
    /// A `deposit` by node: no platform, no text.
    fn deposited(handle: &str, token: Address, sender: Address, amount: u64) -> Self;

    /// A `deposit` by node that moves its sender's refund date to
    /// `refundable_at`.
    fn deposited_until(
        handle: &str,
        token: Address,
        sender: Address,
        amount: u64,
        refundable_at: u64,
    ) -> Self;

    /// A refund of `released` to `sender` from `handle`'s slot.
    fn refunded(handle: &str, sender: Address, round: u64, released: u64) -> Self;
}

impl EscrowEvents for NamesEvent {
    fn deposited(handle: &str, token: Address, sender: Address, amount: u64) -> Self {
        Self::deposited_until(handle, token, sender, amount, REFUNDABLE_AT)
    }

    fn deposited_until(
        handle: &str,
        token: Address,
        sender: Address,
        amount: u64,
        refundable_at: u64,
    ) -> Self {
        Self::Escrow(EscrowEvent::Deposited {
            handle_node: node(handle),
            token,
            sender,
            round: U256::ZERO,
            amount: U256::from(amount),
            refundable_at,
        })
    }

    fn refunded(handle: &str, sender: Address, round: u64, released: u64) -> Self {
        Self::Escrow(EscrowEvent::Refunded {
            handle_node: node(handle),
            token: NATIVE,
            sender,
            recipient: sender,
            round: U256::from(round),
            released: U256::from(released),
            received: U256::from(released),
        })
    }
}

/// What the suite reads off a list of amounts.
trait Amounts {
    /// (token, amount) pairs, in the order served.
    fn token_amounts(&self) -> Vec<(Address, u64)>;
    /// Per token, the amounts the list serves.
    fn by_token(&self) -> BTreeMap<Address, U256>;
    /// (chain, handle node, token) keys, in the order served.
    fn keys(&self) -> Vec<(i64, B256, Address)>;
}

impl Amounts for [EscrowAmount] {
    fn token_amounts(&self) -> Vec<(Address, u64)> {
        self.iter()
            .map(|a| (a.token, a.amount.to::<u64>()))
            .collect()
    }

    fn by_token(&self) -> BTreeMap<Address, U256> {
        self.iter().map(|a| (a.token, a.amount)).collect()
    }

    fn keys(&self) -> Vec<(i64, B256, Address)> {
        self.iter()
            .map(|a| (a.chain_id, a.handle.handle_node, a.token))
            .collect()
    }
}

/// A sender's contributions, as the suite compares them.
trait Contributions {
    /// (token, amount, refund date) triples, in the order served.
    fn contributions(&self) -> Vec<(Address, u64, i64)>;
}

impl Contributions for AddressContributions {
    fn contributions(&self) -> Vec<(Address, u64, i64)> {
        self.amounts
            .iter()
            .map(|c| (c.amount.token, c.amount.amount.to::<u64>(), c.refundable_at))
            .collect()
    }
}

/// What a handle's history says the escrow still owes.
trait Owed {
    /// Per token, what the history deposited less what claims and refunds
    /// released: what the escrow must still hold for the handle. Served
    /// newest first, so it is replayed from its end.
    fn owed(&self) -> BTreeMap<Address, U256>;
}

impl Owed for [HistoryEntry] {
    fn owed(&self) -> BTreeMap<Address, U256> {
        let mut owed = BTreeMap::<Address, U256>::new();
        for entry in self.iter().rev() {
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
}

/// An address history's entries, as the suite compares them.
trait Shape {
    /// (kind, roles, handle node) for each entry, in the order served.
    fn shape(&self) -> Vec<(EventKind, Vec<Role>, Option<B256>)>;
}

impl Shape for AddressHistory {
    fn shape(&self) -> Vec<(EventKind, Vec<Role>, Option<B256>)> {
        self.entries
            .iter()
            .map(|e| {
                let kind = match &e.event {
                    HistoryEvent::IdentityBound { .. } => EventKind::IdentityBound,
                    HistoryEvent::HandlePublished { .. } => EventKind::HandlePublished,
                    HistoryEvent::HandleRetired { .. } => EventKind::HandleRetired,
                    HistoryEvent::HandleUnpublished { .. } => {
                        EventKind::HandleUnpublished
                    }
                    HistoryEvent::ProofVerifierConfigured { .. } => {
                        EventKind::ProofVerifierConfigured
                    }
                    HistoryEvent::CeremonyBound { .. } => EventKind::CeremonyBound,
                    HistoryEvent::BindFeePaid { .. } => EventKind::BindFeePaid,
                    HistoryEvent::Deposited { .. } => EventKind::Deposited,
                    HistoryEvent::HandleRevealed { .. } => EventKind::HandleRevealed,
                    HistoryEvent::Forwarded { .. } => EventKind::Forwarded,
                    HistoryEvent::Claimed { .. } => EventKind::Claimed,
                    HistoryEvent::Refunded { .. } => EventKind::Refunded,
                };
                (
                    kind,
                    e.roles.clone(),
                    e.handle.as_ref().map(|h| h.handle_node),
                )
            })
            .collect()
    }
}

/// What the escrow tests seed on the suite's chain and read back.
trait EscrowSuite {
    /// Six slots over three tokens, one deposit each.
    async fn six_unclaimed_slots(&self);

    /// Every page of a list one row at a time, following each page's cursor
    /// until the last hands out none: the keys in the order served.
    async fn every_page<T: DeserializeOwned + Debug>(
        &self,
        path: &str,
        page: impl Fn(T) -> (Vec<EscrowAmount>, Option<String>),
    ) -> Vec<(i64, B256, Address)>;

    /// The slot's amounts, round included, as `/v1/escrow/node` serves them.
    async fn slot(&self, handle: &str) -> Vec<(U256, U256)>;
}

impl EscrowSuite for Suite {
    async fn six_unclaimed_slots(&self) {
        let payer = Address::repeat_byte(0xC0);
        let deposits = [
            ("h1", USDC, 5),
            ("h2", USDC, 900),
            ("h3", DAI, 40),
            ("h4", NATIVE, 3),
            ("h5", USDC, 60),
            ("h6", DAI, 1),
        ];
        for (block, (handle, token, amount)) in (1u64..).zip(deposits) {
            self.apply(block, NamesEvent::deposited(handle, token, payer, amount))
                .await;
        }
    }

    async fn every_page<T: DeserializeOwned + Debug>(
        &self,
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
            let (amounts, next) = page(self.get(&query).await.answer());
            seen.extend(amounts.keys());
            match next {
                Some(next) => before = Some(next),
                None => return seen,
            }
        }
    }

    async fn slot(&self, handle: &str) -> Vec<(U256, U256)> {
        let held: HandleAmounts = self
            .get(&format!("/v1/escrow/node/{}", node(handle)))
            .await
            .answer();
        held.amounts.iter().map(|a| (a.amount, a.round)).collect()
    }
}

#[tokio::test]
async fn a_handle_collects_before_its_bind_and_its_holder_claims_after() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (bob, carol, dave, dave_wallet) = (
        Address::repeat_byte(0xB0),
        Address::repeat_byte(0xC0),
        Address::repeat_byte(0xD0),
        Address::repeat_byte(0xD1),
    );
    let bob_node = node("bob_1");
    let at = i64::try_from(REFUNDABLE_AT).expect("a date");

    // Nobody holds @bob_1: three deposits escrow, the first to the handle as
    // typed.
    suite
        .apply_tx(
            1,
            vec![
                NamesEvent::deposited("Bob_1", NATIVE, carol, 100),
                NamesEvent::revealed(X, "Bob_1"),
            ],
        )
        .await;
    suite
        .apply(2, NamesEvent::deposited("bob_1", NATIVE, dave, 50))
        .await;
    suite
        .apply(3, NamesEvent::deposited("bob_1", USDC, carol, 7))
        .await;

    // The deposit to the handle disclosed its platform and, folded, its
    // text. Token by token, descending: the native coin's address sorts
    // above USDC's.
    let held: HandleAmounts = suite
        .get(&format!("/v1/escrow/node/{bob_node}"))
        .await
        .answer();
    assert_eq!(held.amounts.token_amounts(), [(NATIVE, 150), (USDC, 7)]);
    assert_eq!(held.handle.platform, Some(KnownPlatform::X));
    assert_eq!(held.handle.handle.as_deref(), Some("bob_1"));
    assert!(held.amounts.iter().all(|a| a.holder.is_none()));
    // By text, hashed here; the node is the same.
    let held: HandleAmounts = suite.get("/v1/escrow/handle/x/BOB_1").await.answer();
    assert_eq!(held.handle.handle_node, bob_node);
    assert_eq!(held.handle.handle.as_deref(), Some("bob_1"));
    // A deposit by node alone names no platform and no text.
    suite
        .apply(20, NamesEvent::deposited("anon_1", DAI, carol, 1))
        .await;
    let anon: HandleAmounts = suite
        .get(&format!("/v1/escrow/node/{}", node("anon_1")))
        .await
        .answer();
    assert_eq!((anon.handle.platform_id, anon.handle.handle), (None, None));
    suite
        .apply(
            21,
            NamesEvent::Escrow(EscrowEvent::Refunded {
                handle_node: node("anon_1"),
                token: DAI,
                sender: carol,
                recipient: carol,
                round: U256::ZERO,
                released: U256::from(1u64),
                received: U256::from(1u64),
            }),
        )
        .await;

    // The senders see what they may take back, and from when.
    let carols: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    assert_eq!(carols.contributions(), [(NATIVE, 100, at), (USDC, 7, at)]);
    let daves: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{dave}"))
        .await
        .answer();
    assert_eq!(daves.contributions(), [(NATIVE, 50, at)]);

    // Dave takes his back, paid to another address of his, through a token
    // that keeps a fee on transfer: the books release what the escrow sent.
    suite
        .apply(
            4,
            NamesEvent::Escrow(EscrowEvent::Refunded {
                handle_node: bob_node,
                token: NATIVE,
                sender: dave,
                recipient: dave_wallet,
                round: U256::ZERO,
                released: U256::from(50u64),
                received: U256::from(49u64),
            }),
        )
        .await;
    let daves: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{dave}"))
        .await
        .answer();
    assert!(daves.amounts.is_empty());

    // Bob binds @bob_1 privately: what is held is his to claim, under the
    // text the deposit disclosed.
    suite
        .apply_tx(5, NamesEvent::bind(bob, X, "222", "bob_1", 5_000, false))
        .await;
    let bobs: AddressAmounts = suite
        .get(&format!("/v1/escrow/claimable/{bob}"))
        .await
        .answer();
    assert_eq!(bobs.amounts.token_amounts(), [(NATIVE, 100), (USDC, 7)]);
    assert!(bobs
        .amounts
        .iter()
        .all(|a| a.holder == Some(bob) && a.handle.handle.as_deref() == Some("bob_1")));
    // The refund released Dave's 50 from the books, not the 49 he received.
    let history: HandleHistory = suite.get("/v1/history/handle/x/bob_1").await.answer();
    assert_eq!(history.entries.owed(), bobs.amounts.by_token());
    // Carol may still refund until he claims, once her date has come: the
    // race is the contract's.
    let carols: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    assert_eq!(carols.contributions(), [(NATIVE, 100, at), (USDC, 7, at)]);
    assert!(carols.amounts.iter().all(|c| c.amount.holder == Some(bob)));

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
    assert_eq!(bobs.amounts.token_amounts(), [(USDC, 7)]);
    let carols: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    assert_eq!(carols.contributions(), [(USDC, 7, at)]);

    // A held handle is paid straight through: nothing escrows.
    suite
        .apply(
            7,
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                handle_node: bob_node,
                token: NATIVE,
                sender: carol,
                holder: bob,
                amount: U256::from(5u64),
                received: U256::from(5u64),
            }),
        )
        .await;

    // The books balance: per token, the escrow holds what was deposited less
    // what claims and refunds released.
    let history: HandleHistory = suite.get("/v1/history/handle/x/bob_1").await.answer();
    let held: HandleAmounts = suite.get("/v1/escrow/handle/x/bob_1").await.answer();
    assert_eq!(history.entries.owed(), held.amounts.by_token());
    assert_eq!(held.amounts.token_amounts(), [(USDC, 7)]);
    assert_eq!(held.amounts[0].round, U256::ZERO);

    // The handle's history spans both sides of the bind, newest first.
    assert!(
        matches!(
            history.entries.events()[..],
            [
                HistoryEvent::Forwarded { .. },
                HistoryEvent::Claimed { .. },
                HistoryEvent::IdentityBound { .. },
                HistoryEvent::Refunded { .. },
                HistoryEvent::Deposited { .. },
                HistoryEvent::Deposited { .. },
                HistoryEvent::HandleRevealed { .. },
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
        history.entries.events()[..],
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
        history.entries.events()[..],
        [
            HistoryEvent::Refunded { .. },
            HistoryEvent::Deposited { .. },
            HistoryEvent::Forwarded { .. },
            HistoryEvent::Claimed { .. },
            HistoryEvent::Deposited { .. },
            HistoryEvent::Deposited { .. },
        ]
    ));
    assert_eq!(history.entries[2].roles, [Role::Sender]);
    assert_eq!(history.entries[3].roles, [Role::Sender]);
    assert_eq!(history.entries[4].roles, [Role::Sender]);
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{dave}"))
        .await
        .answer();
    assert!(matches!(
        history.entries.events()[..],
        [
            HistoryEvent::Refunded { .. },
            HistoryEvent::Deposited { .. }
        ]
    ));
    assert_eq!(history.entries[0].roles, [Role::Sender]);
    assert_eq!(history.entries[1].roles, [Role::Sender]);
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{dave_wallet}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Recipient]);
    assert!(matches!(
        history.entries[0].event,
        HistoryEvent::Refunded { received, .. } if received == U256::from(49u64)
    ));

    // Only USDC is still waiting anywhere on this chain.
    let unclaimed: Unclaimed = suite.get("/v1/escrow/unclaimed").await.answer();
    assert_eq!(unclaimed.amounts.token_amounts(), [(USDC, 7)]);
    suite.done();
}

/// Each deposit moves its own sender's refund date for the sender's whole
/// contribution to the slot, and nobody else's.
#[tokio::test]
async fn a_top_up_moves_only_its_senders_refund_date() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (carol, dave) = (Address::repeat_byte(0xC0), Address::repeat_byte(0xD0));
    let (first, later) = (REFUNDABLE_AT, REFUNDABLE_AT + 600);
    suite
        .apply(
            1,
            NamesEvent::deposited_until("bob_1", NATIVE, carol, 10, first),
        )
        .await;
    suite
        .apply(
            2,
            NamesEvent::deposited_until("bob_1", NATIVE, dave, 20, first),
        )
        .await;
    suite
        .apply(
            3,
            NamesEvent::deposited_until("bob_1", NATIVE, carol, 5, later),
        )
        .await;

    let carols: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    let later = i64::try_from(later).expect("a date");
    assert_eq!(carols.contributions(), [(NATIVE, 15, later)]);
    let daves: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{dave}"))
        .await
        .answer();
    let first = i64::try_from(first).expect("a date");
    assert_eq!(daves.contributions(), [(NATIVE, 20, first)]);
    suite.done();
}

#[tokio::test]
async fn the_unclaimed_list_runs_by_token_then_by_amount() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite.six_unclaimed_slots().await;

    // Token by token, descending, each token's largest amount first: the
    // native coin's address sorts above DAI's, DAI's above USDC's.
    let all: Unclaimed = suite.get("/v1/escrow/unclaimed").await.answer();
    assert_eq!(
        all.amounts.token_amounts(),
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
    suite.six_unclaimed_slots().await;

    let usdc: Unclaimed = suite
        .get(&format!("/v1/escrow/unclaimed?token={USDC}"))
        .await
        .answer();
    assert_eq!(usdc.token, Some(USDC));
    assert_eq!(
        usdc.amounts.token_amounts(),
        [(USDC, 900), (USDC, 60), (USDC, 5)]
    );
    suite.done();
}

#[tokio::test]
async fn every_escrow_list_pages_by_cursor_without_gaps_or_repeats() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (bob, carol) = (Address::repeat_byte(0xB0), Address::repeat_byte(0xC0));
    // Two handles Bob will hold and one nobody does, three tokens each.
    for (block, handle) in (1u64..).step_by(3).zip(["p1", "p2", "p3"]) {
        for (offset, token) in (0u64..).zip([NATIVE, USDC, DAI]) {
            suite
                .apply(
                    block + offset,
                    NamesEvent::deposited(handle, token, carol, 10),
                )
                .await;
        }
    }
    suite
        .apply_tx(20, NamesEvent::bind(bob, X, "1", "p1", 1_000, false))
        .await;
    suite
        .apply_tx(21, NamesEvent::bind(bob, X, "2", "p2", 1_000, false))
        .await;

    let path = format!("/v1/escrow/claimable/{bob}");
    let whole: AddressAmounts = suite.get(&path).await.answer();
    let paged = suite
        .every_page(&path, |p: AddressAmounts| (p.amounts, p.next))
        .await;
    assert_eq!((paged.len(), paged), (6, whole.amounts.keys()));

    let path = format!("/v1/escrow/refundable/{carol}");
    let whole: AddressContributions = suite.get(&path).await.answer();
    let slots = |list: Vec<Contribution>| -> Vec<EscrowAmount> {
        list.into_iter().map(|c| c.amount).collect()
    };
    let paged = suite
        .every_page(&path, |p: AddressContributions| (slots(p.amounts), p.next))
        .await;
    assert_eq!((paged.len(), paged), (9, slots(whole.amounts).keys()));

    let path = format!("/v1/escrow/node/{}", node("p3"));
    let whole: HandleAmounts = suite.get(&path).await.answer();
    let paged = suite
        .every_page(&path, |p: HandleAmounts| (p.amounts, p.next))
        .await;
    assert_eq!((paged.len(), paged), (3, whole.amounts.keys()));

    let path = "/v1/escrow/unclaimed";
    let whole: Unclaimed = suite.get(path).await.answer();
    let paged = suite
        .every_page(path, |p: Unclaimed| (p.amounts, p.next))
        .await;
    assert_eq!((paged.len(), paged), (9, whole.amounts.keys()));
    suite.done();
}

#[tokio::test]
async fn a_history_pages_by_cursor_without_gaps_or_repeats() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let payer = Address::repeat_byte(0xC0);
    // Two events in one transaction and three alone: five entries, two of
    // them sharing a block.
    suite
        .apply_tx(
            1,
            vec![
                NamesEvent::deposited("p1", NATIVE, payer, 1),
                NamesEvent::deposited("p2", NATIVE, payer, 2),
            ],
        )
        .await;
    for block in 2..=4 {
        suite
            .apply(
                block,
                NamesEvent::deposited(&format!("p{}", block + 1), NATIVE, payer, block),
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
    let (alice, bob, carol, dave) = (
        Address::repeat_byte(0xA1),
        Address::repeat_byte(0xB2),
        Address::repeat_byte(0xC3),
        Address::repeat_byte(0xD4),
    );

    // The platform recycled @kim to another account, which Bob proves.
    suite
        .apply_tx(1, NamesEvent::bind(alice, X, "111", "kim", 1_000, false))
        .await;
    suite
        .apply_tx(2, NamesEvent::bind(bob, X, "222", "kim", 2_000, false))
        .await;
    // Carol's account moves to Dave's address, handle and all.
    suite
        .apply_tx(3, NamesEvent::bind(carol, X, "333", "carol", 3_000, false))
        .await;
    suite
        .apply_tx(4, NamesEvent::bind(dave, X, "333", "carol", 4_000, false))
        .await;

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert!(matches!(
        history.entries.events()[..],
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

/// A retirement is in the history of the address that held the handle, which
/// the event names, whoever's bind retired it.
#[tokio::test]
async fn a_retired_handle_is_in_the_history_of_the_address_that_held_it() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, bob) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));

    // Alice binds @kim1, disclosing it, then renames to @kim2: her bind
    // retires @kim1 and withdraws her name for it.
    suite
        .apply_tx(1, NamesEvent::bind(alice, X, "777", "kim1", 1_000, true))
        .await;
    let mut rename = vec![
        NamesEvent::retire(X, "kim1", alice),
        NamesEvent::unpublish(alice),
    ];
    rename.extend(NamesEvent::bind(alice, X, "777", "kim2", 2_000, true));
    suite.apply_tx(2, rename).await;
    // The account moves to Bob's address and renames to @kim3, privately:
    // @kim2 retires from Alice, and her name for it goes.
    let mut to_bob = vec![
        NamesEvent::retire(X, "kim2", alice),
        NamesEvent::unpublish(alice),
    ];
    to_bob.extend(NamesEvent::bind(bob, X, "777", "kim3", 3_000, false));
    suite.apply_tx(3, to_bob).await;

    let (holder, held, moved) = (
        vec![Role::Holder],
        vec![Role::PreviousHandleHolder],
        vec![Role::PreviousIdHolder],
    );
    let row = |kind: EventKind, roles: &Vec<Role>, handle: &str| {
        (kind, roles.clone(), Some(node(handle)))
    };
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert_eq!(
        history.shape(),
        [
            row(EventKind::IdentityBound, &moved, "kim3"),
            row(EventKind::HandleUnpublished, &holder, "kim2"),
            row(EventKind::HandleRetired, &held, "kim2"),
            row(EventKind::HandlePublished, &holder, "kim2"),
            row(EventKind::IdentityBound, &holder, "kim2"),
            row(EventKind::HandleUnpublished, &holder, "kim1"),
            row(EventKind::HandleRetired, &held, "kim1"),
            row(EventKind::HandlePublished, &holder, "kim1"),
            row(EventKind::IdentityBound, &holder, "kim1"),
        ]
    );
    // Bob never held @kim2: his history is his own bind.
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{bob}"))
        .await
        .answer();
    assert_eq!(
        history.shape(),
        [row(EventKind::IdentityBound, &holder, "kim3")]
    );
    suite.done();
}

/// An index that starts after a node's bind still learns its platform from
/// a retire, which names it.
#[tokio::test]
async fn a_retire_names_the_platform_of_a_node_whose_bind_was_missed() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite
        .apply_tx(
            1,
            vec![NamesEvent::retire(X, "kim1", Address::repeat_byte(0xA1))],
        )
        .await;
    let history: HandleHistory = suite
        .get(&format!("/v1/history/node/{}", node("kim1")))
        .await
        .answer();
    assert_eq!(history.handle.platform, Some(X));
    assert_eq!(history.handle.platform_id, Some(X.id()));
    suite.done();
}

#[tokio::test]
async fn a_bind_fee_is_in_the_payers_history_under_the_handle_it_bought() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (alice, app) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xFE));
    let digest = B256::repeat_byte(0xD1);
    // One bind's transaction: the binding, its ceremony, its fee.
    let mut tx = NamesEvent::bind(alice, X, "111", "ann_1", 1_000, false);
    tx.extend([
        NamesEvent::CeremonyBound {
            authorization_digest: digest,
            holder: alice,
            platform_id: X.id(),
            client_identifier: Bytes::from_static(b"app"),
        },
        NamesEvent::BindFeePaid {
            authorization_digest: digest,
            receiver: app,
            amount: U256::from(1_000u64),
        },
    ]);
    suite.apply_tx(1, tx).await;

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert!(matches!(
        history.entries.events()[..],
        [
            HistoryEvent::BindFeePaid { amount, .. },
            HistoryEvent::CeremonyBound { .. },
            HistoryEvent::IdentityBound { .. },
        ] if *amount == U256::from(1_000u64)
    ));
    assert!(history.entries.iter().all(|e| e.roles == [Role::Holder]
        && e.handle.as_ref().map(|h| h.handle_node) == Some(node("ann_1"))));

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{app}"))
        .await
        .answer();
    assert!(matches!(
        history.entries.events()[..],
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
    let alice = Address::repeat_byte(0xA1);
    suite
        .apply_tx(1, NamesEvent::bind(alice, X, "111", "ann_1", 1_000, true))
        .await;
    suite.apply(2, NamesEvent::unpublish(alice)).await;

    let history: HandleHistory = suite.get("/v1/history/handle/x/ann_1").await.answer();
    assert!(matches!(
        history.entries.events()[..],
        [
            HistoryEvent::HandleUnpublished { .. },
            HistoryEvent::HandlePublished { .. },
            HistoryEvent::HandleRevealed { .. },
            HistoryEvent::IdentityBound { .. },
        ]
    ));
    // Withdrawn: the history still shows the node and its disclosed text.
    assert!(history.entries.iter().all(|e| e
        .handle
        .as_ref()
        .is_some_and(
            |h| h.handle_node == node("ann_1") && h.handle.as_deref() == Some("ann_1")
        )));
    suite.done();
}

#[tokio::test]
async fn an_unpublish_with_nothing_published_concerns_no_handle() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let alice = Address::repeat_byte(0xA1);
    suite
        .apply_tx(1, NamesEvent::bind(alice, X, "111", "ann_1", 1_000, false))
        .await;
    suite.apply(2, NamesEvent::unpublish(alice)).await;

    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert!(matches!(
        history.entries.events()[..],
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
        .apply_tx(
            1,
            NamesEvent::bind(Address::repeat_byte(1), X, "1", "somebody", 1_000, false),
        )
        .await;
    let held: HandleAmounts = suite.get("/v1/escrow/handle/x/nobody").await.answer();
    assert!(held.amounts.is_empty() && held.next.is_none());
    let history: HandleHistory = suite.get("/v1/history/handle/x/nobody").await.answer();
    assert!(history.entries.is_empty() && history.next.is_none());
    suite.done();
}

/// A handle's platform and text come from what the chain named and
/// disclosed about its node, not from the page: asked by node, a disclosed
/// handle shows them on an escrow page nothing was sent to and on a history
/// page past its events.
#[tokio::test]
async fn an_empty_page_still_names_the_handle_it_is_about() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite
        .apply_tx(
            1,
            NamesEvent::bind(Address::repeat_byte(1), X, "1", "alice", 1_000, true),
        )
        .await;
    let held: HandleAmounts = suite
        .get(&format!("/v1/escrow/node/{}", node("alice")))
        .await
        .answer();
    assert!(held.amounts.is_empty() && held.next.is_none());
    assert_eq!(held.handle.platform, Some(X));
    assert_eq!(held.handle.handle.as_deref(), Some("alice"));

    // A page from before the first event.
    let history: HandleHistory = suite
        .get(&format!(
            "/v1/history/node/{}?before=0-0-0-0",
            node("alice")
        ))
        .await
        .answer();
    assert!(history.entries.is_empty() && history.next.is_none());
    assert_eq!(history.handle, held.handle);
    suite.done();
}

/// The chain folds a deposit's text and checks nothing else, so text no
/// circuit hashes still finds what was sent to it, but no account can bind
/// it and its text is not stored.
#[tokio::test]
async fn text_finds_a_deposit_the_way_the_chain_hashed_it() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let carol = Address::repeat_byte(0xC0);
    let (_, quoted) = X.folded_handle_node("Bo\"b");
    assert!(X.handle_node("Bo\"b").is_err(), "no circuit hashes a quote");
    suite
        .apply_tx(
            1,
            vec![
                NamesEvent::Escrow(EscrowEvent::Deposited {
                    handle_node: quoted,
                    token: NATIVE,
                    sender: carol,
                    round: U256::ZERO,
                    amount: U256::from(9u64),
                    refundable_at: REFUNDABLE_AT,
                }),
                NamesEvent::revealed(X, "Bo\"b"),
            ],
        )
        .await;

    let held: HandleAmounts = suite.get("/v1/escrow/handle/x/bo%22b").await.answer();
    assert_eq!(held.handle.handle_node, quoted);
    assert_eq!(held.handle.handle, None);
    assert_eq!(held.amounts.token_amounts(), [(NATIVE, 9)]);
    let history: HandleHistory = suite.get("/v1/history/handle/x/bo%22b").await.answer();
    assert_eq!(history.entries.len(), 2);
    suite.done();
}

#[tokio::test]
async fn the_status_names_the_escrow_the_chain_was_indexed_from() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (registry, escrow) = (Address::repeat_byte(0x11), Address::repeat_byte(0x22));
    let mut lease = suite.store().acquire_writer().await.expect("lease");
    suite
        .store()
        .prepare(
            &mut lease,
            Contracts {
                registry,
                escrow: Some(escrow),
            },
        )
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
    lease.release().await.expect("release");
    suite.done();
}

#[tokio::test]
async fn another_escrow_replays_the_chain_and_the_same_one_keeps_it() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let registry = Address::repeat_byte(0x11);
    let (escrow, other_escrow) = (Address::repeat_byte(0x22), Address::repeat_byte(0x33));
    let mut lease = suite.store().acquire_writer().await.expect("lease");
    let writer = suite
        .store()
        .prepare(
            &mut lease,
            Contracts {
                registry,
                escrow: Some(escrow),
            },
        )
        .await
        .expect("prepare");
    let payer = Address::repeat_byte(0xC0);
    writer
        .apply(1, NamesEvent::deposited("h1", NATIVE, payer, 1))
        .await;

    suite
        .store()
        .prepare(
            &mut lease,
            Contracts {
                registry,
                escrow: Some(escrow),
            },
        )
        .await
        .expect("prepare again");
    let kept: Unclaimed = suite.get("/v1/escrow/unclaimed").await.answer();
    assert_eq!(kept.amounts.token_amounts(), [(NATIVE, 1)]);

    // Another escrow clears the chain, cursor included: nothing is served
    // until the replay commits its first window.
    suite
        .store()
        .prepare(
            &mut lease,
            Contracts {
                registry,
                escrow: Some(other_escrow),
            },
        )
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
    lease.release().await.expect("release");
    suite.done();
}

/// A ceremony and a fee take the handle and payer of the bind before them in
/// their own transaction only: in another transaction of the same window
/// they concern no handle and charge no holder.
#[tokio::test]
async fn a_ceremony_and_a_fee_take_only_their_own_transactions_bind() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let alice = Address::repeat_byte(0xA1);
    let at = |log_index, tx: u8| LogPosition {
        block_number: 1,
        log_index,
        tx_hash: B256::repeat_byte(tx),
        block_time: 1_700_000_001,
    };
    let mut window = suite.writer.begin_window().await.expect("a window");
    for (log_index, event) in
        (0u64..).zip(NamesEvent::bind(alice, X, "111", "ann_1", 1_000, true))
    {
        window
            .apply(&event, &at(log_index, 0x0A))
            .await
            .expect("the bind");
    }
    // Another holder's ceremony and fee, in another transaction.
    let dave = Address::repeat_byte(0xD0);
    let ceremony = NamesEvent::CeremonyBound {
        authorization_digest: B256::repeat_byte(0xD1),
        holder: dave,
        platform_id: X.id(),
        client_identifier: Bytes::from_static(b"app"),
    };
    window
        .apply(&ceremony, &at(9, 0x0B))
        .await
        .expect("the ceremony");
    let fee = NamesEvent::BindFeePaid {
        authorization_digest: B256::repeat_byte(0xD1),
        receiver: Address::repeat_byte(0xFE),
        amount: U256::from(5u64),
    };
    window.apply(&fee, &at(10, 0x0B)).await.expect("the fee");
    window.commit(1).await.expect("commit");

    let stray = |event: &&HistoryEvent| {
        matches!(
            event,
            HistoryEvent::CeremonyBound { .. } | HistoryEvent::BindFeePaid { .. }
        )
    };
    let history: HandleHistory = suite.get("/v1/history/handle/x/ann_1").await.answer();
    assert!(!history.entries.events().iter().any(stray), "{history:?}");
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert!(!history.entries.events().iter().any(stray), "{history:?}");
    // The ceremony stays its own holder's, concerning no handle.
    let history: AddressHistory = suite
        .get(&format!("/v1/history/address/{dave}"))
        .await
        .answer();
    assert!(
        matches!(
            &history.entries[..],
            [entry] if matches!(entry.event, HistoryEvent::CeremonyBound { .. })
                && entry.handle.is_none()
        ),
        "{history:?}"
    );
    suite.done();
}

/// A claim opens its slot's next round: after the holder claims and leaves
/// the handle, a deposit to it is held alone under that round, refundable to
/// its own sender.
#[tokio::test]
async fn a_claim_opens_the_next_round() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (bob, carol, dave) = (
        Address::repeat_byte(0xB0),
        Address::repeat_byte(0xC0),
        Address::repeat_byte(0xD0),
    );
    suite
        .apply(1, NamesEvent::deposited("r1", NATIVE, carol, 10))
        .await;
    suite
        .apply_tx(2, NamesEvent::bind(bob, X, "222", "r1", 1_000, false))
        .await;
    let claim = NamesEvent::Escrow(EscrowEvent::Claimed {
        handle_node: node("r1"),
        token: NATIVE,
        claimer: bob,
        recipient: bob,
        round: U256::ZERO,
        released: U256::from(10u64),
        received: U256::from(10u64),
    });
    suite.apply(3, claim).await;
    // Bob's account moves to another handle, which retires @r1: a deposit to
    // it is escrowed again, under the round the claim opened.
    let mut rename = vec![NamesEvent::retire(X, "r1", bob)];
    rename.extend(NamesEvent::bind(bob, X, "222", "r2", 2_000, false));
    suite.apply_tx(4, rename).await;
    let next = NamesEvent::Escrow(EscrowEvent::Deposited {
        handle_node: node("r1"),
        token: NATIVE,
        sender: dave,
        round: U256::from(1u64),
        amount: U256::from(4u64),
        refundable_at: REFUNDABLE_AT,
    });
    suite.apply(5, next).await;

    let held: HandleAmounts = suite
        .get(&format!("/v1/escrow/node/{}", node("r1")))
        .await
        .answer();
    let slot = &held.amounts[..];
    assert_eq!(slot.len(), 1, "{held:?}");
    assert_eq!(
        (slot[0].amount, slot[0].round),
        (U256::from(4u64), U256::from(1u64))
    );
    let daves: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{dave}"))
        .await
        .answer();
    let refundable_at = i64::try_from(REFUNDABLE_AT).expect("a date");
    assert_eq!(daves.contributions(), [(NATIVE, 4, refundable_at)]);
    assert_eq!(daves.amounts[0].amount.round, U256::from(1u64));
    let carols: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{carol}"))
        .await
        .answer();
    assert!(carols.amounts.is_empty(), "{carols:?}");
    suite.done();
}

/// An index that starts after a slot's first claims meets it in a later
/// round: the slot keeps the round its first deposit names.
#[tokio::test]
async fn a_late_start_keeps_the_round_its_first_deposit_names() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let late = NamesEvent::Escrow(EscrowEvent::Deposited {
        handle_node: node("late"),
        token: NATIVE,
        sender: Address::repeat_byte(0xC0),
        round: U256::from(3u64),
        amount: U256::from(1u64),
        refundable_at: REFUNDABLE_AT,
    });
    suite.apply(1, late).await;
    assert_eq!(
        suite.slot("late").await,
        [(U256::from(1u64), U256::from(3u64))]
    );
    suite.done();
}

/// A refund takes back the sender's contribution as the index booked it.
/// Part of it came before the index started, so the refund releases more,
/// and the slot keeps exactly what the other senders put in, as on chain.
#[tokio::test]
async fn a_refund_takes_back_only_what_the_index_booked_for_its_sender() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (carol, dave) = (Address::repeat_byte(0xC0), Address::repeat_byte(0xD0));
    suite
        .apply(1, NamesEvent::deposited("early", NATIVE, carol, 50))
        .await;
    suite
        .apply(2, NamesEvent::deposited("early", NATIVE, dave, 5))
        .await;
    // Dave also put in 10 before the index started: his refund releases 15.
    suite
        .apply(3, NamesEvent::refunded("early", dave, 0, 15))
        .await;
    assert_eq!(suite.slot("early").await, [(U256::from(50u64), U256::ZERO)]);
    let daves: AddressContributions = suite
        .get(&format!("/v1/escrow/refundable/{dave}"))
        .await
        .answer();
    assert!(daves.amounts.is_empty(), "{daves:?}");
    suite.done();
}

/// A refund that releases less than the index booked for its sender is one
/// the contract never makes: the window fails rather than hide it.
#[tokio::test]
async fn a_refund_releasing_less_than_was_booked_fails_the_window() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let dave = Address::repeat_byte(0xD0);
    suite
        .apply(1, NamesEvent::deposited("short", NATIVE, dave, 10))
        .await;
    let at = LogPosition {
        block_number: 2,
        log_index: 0,
        tx_hash: B256::repeat_byte(0x02),
        block_time: 1_700_000_002,
    };
    let mut window = suite.writer.begin_window().await.expect("a window");
    let refused = window
        .apply(&NamesEvent::refunded("short", dave, 0, 5), &at)
        .await;
    assert!(
        matches!(refused, Err(ApplyError::Books { .. })),
        "{refused:?}"
    );
    drop(window);
    suite.done();
}
