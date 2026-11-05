//! Referrals and what waits for a holder, end to end: decoded events go
//! through the apply path the indexer uses, and every assertion goes through
//! the axum handlers a caller hits.
//!
//! Skips silently when `DATABASE_URL` is unset; everything past that check
//! panics on failure. The suite's chains and handles are its own.

use alloy::primitives::{
    address,
    Address,
    B256,
    U256,
};
use tokio::sync::Mutex;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressReferrals,
        Delivery,
        Outcome,
        Referral,
        Status,
    },
    db::{
        ApplyError,
        ChainStore,
        ChainWriter,
    },
    events::{
        EscrowEvent,
        LogPosition,
        NamesEvent,
    },
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    Events,
    Reads,
    Seed,
    Suite,
    Writes,
    SPLITTER,
};

const CHAIN: i64 = 31601;
const OTHER_CHAIN: i64 = 31602;

static SUITE: Mutex<()> = Mutex::const_new(());

const X: KnownPlatform = KnownPlatform::X;
const NATIVE: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

const ALICE: Address = Address::repeat_byte(0xA1);
const BOB: Address = Address::repeat_byte(0xB2);
const CAROL: Address = Address::repeat_byte(0xC3);
const DAVE: Address = Address::repeat_byte(0xD4);
const ERIN: Address = Address::repeat_byte(0xE6);

/// What the suite's binds pay, and the referrer's share of it at the rate:
/// the splitter reports the share whatever became of it.
const FEE: u64 = 1_000;
const SHARE: u64 = 300;

fn node(handle: &str) -> B256 {
    X.handle_node(handle).expect("an X handle")
}

/// The decimal user id the suite binds `handle`'s account under.
fn id(handle: &str) -> String {
    let head: [u8; 4] = node(handle)[..4].try_into().expect("four bytes");
    u32::from_be_bytes(head).to_string()
}

/// A bind that names a referrer, as the suite writes it.
struct Referred<'a> {
    binder: Address,
    handle: &'a str,
    referrer: &'a str,
    outcome: Outcome,
    delivery: Option<Delivery>,
}

impl<'a> Referred<'a> {
    /// A bind whose share the escrow took, delivering it as `delivery`.
    fn paid(
        binder: Address,
        handle: &'a str,
        referrer: &'a str,
        delivery: Delivery,
    ) -> Self {
        Self {
            binder,
            handle,
            referrer,
            outcome: Outcome::Paid,
            delivery: Some(delivery),
        }
    }

    /// A bind whose share went to the treasury.
    fn unpaid(
        binder: Address,
        handle: &'a str,
        referrer: &'a str,
        outcome: Outcome,
    ) -> Self {
        Self {
            binder,
            handle,
            referrer,
            outcome,
            delivery: None,
        }
    }

    /// Its events in the order the splitter's transaction emits them: the
    /// bind, the escrow's answer to the splitter's deposit when one was
    /// paid, and the `Referred`.
    fn events(&self) -> Vec<NamesEvent> {
        let Self {
            binder,
            handle,
            referrer,
            outcome,
            delivery,
        } = *self;
        let mut events = NamesEvent::bind(binder, X, &id(handle), handle, 1, true);
        events.extend(
            delivery.map(|delivery| NamesEvent::splitter_deposit(referrer, delivery)),
        );
        events.push(NamesEvent::referred(referrer, binder, handle, outcome));
        events
    }
}

/// The suite's two chains, emptied, and the right to write each.
struct Referrals {
    suite: Suite,
    other: ChainWriter,
}

impl Referrals {
    async fn open() -> Option<Self> {
        let suite = Suite::open(&SUITE, CHAIN).await?;
        let other = ChainStore::new(suite.pool.clone(), OTHER_CHAIN);
        other.forget().await;
        let other = other.writer().await;
        Some(Self { suite, other })
    }

    /// The chain's writer: the suite's own, or the other chain's.
    fn on(&self, chain: i64) -> &ChainWriter {
        if chain == CHAIN {
            &self.suite.writer
        } else {
            &self.other
        }
    }

    /// `holder` binds `handle` on `chain` at `block`, disclosing it as its
    /// name, with nobody referring it.
    async fn bind(&self, chain: i64, block: u64, handle: &str, holder: Address) {
        self.on(chain)
            .apply_tx(
                block,
                NamesEvent::bind(holder, X, &id(handle), handle, 1, true),
            )
            .await;
    }

    /// `bind`, in its own transaction on `chain` at `block`.
    async fn referred(&self, chain: i64, block: u64, bind: Referred<'_>) {
        self.on(chain).apply_tx(block, bind.events()).await;
    }

    /// The referrals of `holder`, across every chain.
    async fn referrals(&self, holder: Address) -> AddressReferrals {
        self.suite
            .pool
            .get(&format!("/v1/referrals/address/{holder}"))
            .await
            .answer()
    }

    /// What waits for `holder`, across every chain.
    async fn awaiting(&self, holder: Address) -> AddressAmounts {
        self.suite
            .pool
            .get(&format!("/v1/escrow/awaiting/{holder}"))
            .await
            .answer()
    }

    /// What `holder` may claim, across every chain.
    async fn claimable(&self, holder: Address) -> AddressAmounts {
        self.suite
            .pool
            .get(&format!("/v1/escrow/claimable/{holder}"))
            .await
            .answer()
    }

    fn done(self) {
        self.suite.done();
    }
}

/// The splitter's events, as the suite writes them.
trait SplitterEvents {
    /// The escrow's answer to the splitter's deposit of the share to
    /// `referrer`: forwarded to Alice, its holder there, or held for its
    /// claim.
    fn splitter_deposit(referrer: &str, delivery: Delivery) -> Self;

    /// An escrow deposit to `handle` by `sender`.
    fn deposit_by(handle: &str, sender: Address, amount: u64) -> Self;

    /// The splitter's `Referred` for `binder`'s bind of `handle`.
    fn referred(referrer: &str, binder: Address, handle: &str, outcome: Outcome) -> Self;
}

impl SplitterEvents for NamesEvent {
    fn splitter_deposit(referrer: &str, delivery: Delivery) -> Self {
        match delivery {
            Delivery::Forwarded => Self::Escrow(EscrowEvent::Forwarded {
                handle_node: node(referrer),
                token: NATIVE,
                sender: SPLITTER,
                holder: ALICE,
                amount: U256::from(SHARE),
                received: U256::from(SHARE),
            }),
            Delivery::Held => Self::deposit_by(referrer, SPLITTER, SHARE),
        }
    }

    fn deposit_by(handle: &str, sender: Address, amount: u64) -> Self {
        Self::Escrow(EscrowEvent::Deposited {
            handle_node: node(handle),
            token: NATIVE,
            sender,
            round: U256::ZERO,
            amount: U256::from(amount),
            refundable_at: 1_800_000_000,
        })
    }

    fn referred(referrer: &str, binder: Address, handle: &str, outcome: Outcome) -> Self {
        Self::Referred {
            referrer_node: node(referrer),
            binder,
            session_id: B256::repeat_byte(0x5E),
            referrer_platform_id: X.id(),
            platform_id: X.id(),
            handle_node: node(handle),
            fee: U256::from(FEE),
            share: U256::from(SHARE),
            outcome,
        }
    }
}

/// One referral as the suite compares it: chain, binder, the binder's
/// handle, outcome and delivery.
type Seen<'a> = (i64, Address, Option<&'a str>, Outcome, Option<Delivery>);

/// What the suite compares of a list of referrals.
trait Shape {
    /// Each referral, in the order served.
    fn shape(&self) -> Vec<Seen<'_>>;
}

impl Shape for [Referral] {
    fn shape(&self) -> Vec<Seen<'_>> {
        self.iter()
            .map(|r| {
                (
                    r.chain_id,
                    r.binder,
                    r.binder_handle.as_deref(),
                    r.outcome,
                    r.delivery,
                )
            })
            .collect()
    }
}

#[tokio::test]
async fn a_paid_share_is_listed_as_forwarded_or_held_where_it_went() {
    let Some(suite) = Referrals::open().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite.bind(CHAIN, 1, "alice_r", ALICE).await;
    // On this chain Alice holds her handle, so her share is forwarded; on
    // the other nobody does, so it is held for whoever binds it there.
    let (forwarded, held) = (Delivery::Forwarded, Delivery::Held);
    suite
        .referred(CHAIN, 2, Referred::paid(BOB, "bob_r", "alice_r", forwarded))
        .await;
    suite
        .referred(
            OTHER_CHAIN,
            3,
            Referred::paid(CAROL, "carol_r", "alice_r", held),
        )
        .await;

    let referrals = suite.referrals(ALICE).await;
    assert_eq!(
        referrals.referrals.shape(),
        [
            (
                OTHER_CHAIN,
                CAROL,
                Some("carol_r"),
                Outcome::Paid,
                Some(held)
            ),
            (CHAIN, BOB, Some("bob_r"), Outcome::Paid, Some(forwarded)),
        ]
    );
    assert!(referrals
        .referrals
        .iter()
        .all(|r| r.fee == U256::from(FEE) && r.share == U256::from(SHARE)));
    let first = &referrals.referrals[0];
    assert_eq!(first.referrer.handle_node, node("alice_r"));
    assert_eq!(first.referrer.platform, Some(X));
    assert_eq!(first.referrer.handle.as_deref(), Some("alice_r"));
    assert_eq!(referrals.next, None);

    // The held share waits for Alice on the chain where nobody holds her
    // handle; the forwarded one left the escrow.
    let awaiting = suite.awaiting(ALICE).await;
    assert_eq!(
        awaiting
            .amounts
            .iter()
            .map(|a| (
                a.chain_id,
                a.handle.handle_node,
                a.holder,
                a.amount.to::<u64>()
            ))
            .collect::<Vec<_>>(),
        [(OTHER_CHAIN, node("alice_r"), None, SHARE)]
    );
    assert_eq!(suite.claimable(ALICE).await.amounts, []);

    // Nobody else's referrals are Alice's, and the status names the splitter.
    assert_eq!(suite.referrals(BOB).await.referrals, []);
    let status: Status = suite.suite.pool.get("/v1/status").await.answer();
    assert!(status
        .chains
        .iter()
        .filter(|c| [CHAIN, OTHER_CHAIN].contains(&c.chain_id))
        .all(|c| c.referral_splitter == Some(SPLITTER)));
    suite.done();
}

#[tokio::test]
async fn a_self_referral_and_a_refused_share_are_listed_without_a_delivery() {
    let Some(suite) = Referrals::open().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite.bind(CHAIN, 1, "alice_s", ALICE).await;
    let own = Referred::unpaid(ALICE, "alice_s2", "alice_s", Outcome::SelfReferral);
    suite.referred(CHAIN, 2, own).await;
    let refused = Referred::unpaid(DAVE, "dave_s", "alice_s", Outcome::Refused);
    suite.referred(CHAIN, 3, refused).await;

    assert_eq!(
        suite.referrals(ALICE).await.referrals.shape(),
        [
            (CHAIN, DAVE, Some("dave_s"), Outcome::Refused, None),
            (CHAIN, ALICE, Some("alice_s2"), Outcome::SelfReferral, None),
        ]
    );
    suite.done();
}

/// Two referred binds in one transaction: each `Referred` takes the
/// splitter's deposit just before it, for its own referrer.
#[tokio::test]
async fn two_referred_binds_in_one_transaction_pair_each_with_its_own_deposit() {
    let Some(suite) = Referrals::open().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite.bind(CHAIN, 1, "alice_t", ALICE).await;
    suite.bind(OTHER_CHAIN, 2, "dave_t", DAVE).await;
    let (forwarded, held) = (Delivery::Forwarded, Delivery::Held);
    let mut events = Referred::paid(BOB, "bob_t", "alice_t", forwarded).events();
    events.extend(Referred::paid(CAROL, "carol_t", "dave_t", held).events());
    suite.suite.apply_tx(3, events).await;

    assert_eq!(
        suite.referrals(ALICE).await.referrals.shape(),
        [(CHAIN, BOB, Some("bob_t"), Outcome::Paid, Some(forwarded))]
    );
    assert_eq!(
        suite.referrals(DAVE).await.referrals.shape(),
        [(CHAIN, CAROL, Some("carol_t"), Outcome::Paid, Some(held))]
    );
    suite.done();
}

/// A paid share pairs only with the splitter's own deposit, in its
/// transaction and for its referrer; without one the window fails and
/// nothing of it is written.
#[tokio::test]
async fn a_paid_share_without_the_splitters_deposit_fails_the_window() {
    let Some(suite) = Referrals::open().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite.bind(CHAIN, 1, "alice_u", ALICE).await;
    let at = |block: u64, log_index: u64, tx: u8| LogPosition {
        block_number: block,
        log_index,
        tx_hash: B256::repeat_byte(tx),
        block_time: 1_700_000_000 + block,
    };
    let paid = NamesEvent::referred("alice_u", BOB, "bob_u", Outcome::Paid);
    let unpaired = |result: Result<bool, ApplyError>| {
        matches!(result, Err(ApplyError::UnpairedReferral { .. }))
    };

    // A deposit by any other sender is not the splitter's.
    let mut window = suite.suite.writer.begin_window().await.expect("a window");
    let by_another = NamesEvent::deposit_by("alice_u", ERIN, SHARE);
    window
        .apply(&by_another, &at(2, 0, 0x02))
        .await
        .expect("a deposit");
    assert!(unpaired(window.apply(&paid, &at(2, 1, 0x02)).await));
    drop(window);

    // The splitter's deposit for another node is not this share's.
    let mut window = suite.suite.writer.begin_window().await.expect("a window");
    let other_node = NamesEvent::splitter_deposit("dave_u", Delivery::Held);
    window
        .apply(&other_node, &at(2, 0, 0x02))
        .await
        .expect("a deposit");
    assert!(unpaired(window.apply(&paid, &at(2, 1, 0x02)).await));
    drop(window);

    // Nor is one from an earlier transaction.
    let mut window = suite.suite.writer.begin_window().await.expect("a window");
    let earlier = NamesEvent::splitter_deposit("alice_u", Delivery::Held);
    window
        .apply(&earlier, &at(2, 0, 0x02))
        .await
        .expect("a deposit");
    assert!(unpaired(window.apply(&paid, &at(2, 1, 0x03)).await));
    drop(window);

    assert_eq!(suite.referrals(ALICE).await.referrals, []);
    suite.done();
}

/// Awaiting lists a slot only on a chain where nobody holds the node: once
/// someone binds it there, the slot is that holder's claim.
#[tokio::test]
async fn a_slot_awaits_the_holder_only_where_nobody_holds_its_handle() {
    let Some(suite) = Referrals::open().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let payer = Address::repeat_byte(0xF0);
    suite.bind(CHAIN, 1, "alice_w", ALICE).await;
    suite
        .suite
        .apply(2, NamesEvent::deposit_by("alice_w", payer, 7))
        .await;
    suite
        .other
        .apply(3, NamesEvent::deposit_by("alice_w", payer, 11))
        .await;
    suite
        .other
        .apply(4, NamesEvent::deposit_by("nobody_w", payer, 13))
        .await;

    let amounts = |answer: AddressAmounts| {
        answer
            .amounts
            .iter()
            .map(|a| (a.chain_id, a.amount.to::<u64>()))
            .collect::<Vec<_>>()
    };
    assert_eq!(amounts(suite.awaiting(ALICE).await), [(OTHER_CHAIN, 11)]);
    assert_eq!(amounts(suite.claimable(ALICE).await), [(CHAIN, 7)]);

    // Erin binds the handle on the other chain: the slot is Erin's claim,
    // and waits for Alice no longer.
    suite.bind(OTHER_CHAIN, 5, "alice_w", ERIN).await;
    assert_eq!(suite.awaiting(ALICE).await.amounts, []);
    assert_eq!(amounts(suite.claimable(ERIN).await), [(OTHER_CHAIN, 11)]);
    suite.done();
}

/// A holder of the same handle on two chains sees each referral once, and
/// every page of one row follows the full list's order.
#[tokio::test]
async fn referrals_are_listed_once_per_node_and_page_in_order() {
    let Some(suite) = Referrals::open().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite.bind(CHAIN, 1, "alice_p", ALICE).await;
    suite.bind(OTHER_CHAIN, 2, "alice_p", ALICE).await;
    suite.bind(CHAIN, 3, "alice_p2", ALICE).await;
    // Alice holds @alice_p on both chains and @alice_p2 only on this one.
    let (forwarded, held) = (Delivery::Forwarded, Delivery::Held);
    let binds = [
        (CHAIN, Referred::paid(BOB, "bob_p", "alice_p", forwarded)),
        (
            OTHER_CHAIN,
            Referred::paid(CAROL, "carol_p", "alice_p", forwarded),
        ),
        (CHAIN, Referred::paid(DAVE, "dave_p", "alice_p2", forwarded)),
        (
            OTHER_CHAIN,
            Referred::paid(ERIN, "erin_p", "alice_p2", held),
        ),
    ];
    for (block, (chain, bind)) in (4u64..).zip(binds) {
        suite.referred(chain, block, bind).await;
    }

    let all = suite.referrals(ALICE).await;
    let binders =
        |referrals: &[Referral]| referrals.iter().map(|r| r.binder).collect::<Vec<_>>();
    assert_eq!(binders(&all.referrals), [ERIN, DAVE, CAROL, BOB]);

    let mut paged = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let path = match &before {
            Some(cursor) => {
                format!("/v1/referrals/address/{ALICE}?limit=1&before={cursor}")
            }
            None => format!("/v1/referrals/address/{ALICE}?limit=1"),
        };
        let page: AddressReferrals = suite.suite.pool.get(&path).await.answer();
        paged.extend(binders(&page.referrals));
        match page.next {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    assert_eq!(paged, [ERIN, DAVE, CAROL, BOB]);

    let on_other: AddressReferrals = suite
        .suite
        .pool
        .get_on(OTHER_CHAIN, &format!("/v1/referrals/address/{ALICE}"))
        .await
        .answer();
    assert_eq!(binders(&on_other.referrals), [ERIN, CAROL]);
    suite.done();
}
