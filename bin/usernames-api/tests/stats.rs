//! `GET /v1/stats`, `GET /v1/stats/days` and `GET /v1/history/chain/{chain}`
//! over a journal written through the indexer's apply path: users, links,
//! fees and escrow movements counted per chain, in total and per UTC day,
//! the same whether folded window by window or whole, users counted once
//! across chains in whatever order the chains fold, tokens and fee
//! receivers nobody listed counted together, 30 days to a page, one answer
//! served until it expires, and one day's events listed page by page.
//!
//! Skips without `DATABASE_URL`, unless `CI` is set: there its absence
//! fails. The suite's chain is its own; the answer lists every chain the
//! database holds, so it reads only its own. Each test forgets its chain
//! when it ends: the index-plan test plans over what the database holds,
//! and escrow rows left here can tip its plans.

use std::ops::Range;

use alloy::primitives::{
    address,
    Address,
    B256,
    U256,
    U512,
};
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
    Router,
};
use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tokio::{
    sync::Mutex,
    task::JoinSet,
};
use tower::ServiceExt;
use usernames_core::{
    api::{
        model::{
            Activity,
            AllChainsDay,
            ChainHistory,
            ChainStats,
            DayActivity,
            EscrowActivity,
            FeesPaid,
            OtherFees,
            OtherTokens,
            PlatformHandles,
            PlatformLinks,
            RegistrationFees,
            Stats,
            StatsDays,
            TokenFlow,
            TokenFlows,
        },
        AppState,
    },
    db::{
        ChainStore,
        ListedAddress,
        Listing,
        Store,
    },
    events::{
        EscrowEvent,
        NamesEvent,
        Outcome,
    },
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    Events,
    LockWaits,
    Seed,
    Suite,
    Writes,
    SPLITTER,
};

const CHAIN: i64 = 31351;
/// The second chain of the case across chains.
const OTHER_CHAIN: i64 = 31352;

static SUITE: Mutex<()> = Mutex::const_new(());

const X: KnownPlatform = KnownPlatform::X;
const GITHUB: KnownPlatform = KnownPlatform::GitHub;

/// The chain's own coin, as the escrow names it (EIP-7528).
const NATIVE: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
const USDC: Address = Address::repeat_byte(0x05);

const ALICE: Address = Address::repeat_byte(0xA1);
const BOB: Address = Address::repeat_byte(0xB0);
const DAVE: Address = Address::repeat_byte(0xDA);
const EVE: Address = Address::repeat_byte(0xE0);
const FRANK: Address = Address::repeat_byte(0xF1);
const GRACE: Address = Address::repeat_byte(0x6A);
const SENDER: Address = Address::repeat_byte(0x51);
const PAYER: Address = Address::repeat_byte(0x52);
const SAFE: Address = Address::repeat_byte(0xFE);
/// Wallets on both of the suite's chains, and on no other suite's.
const ROAMER: Address = Address::repeat_byte(0x7C);
const NOMAD: Address = Address::repeat_byte(0x7D);
/// A wallet on no other suite's chains: the users across chains count it
/// once, whatever other suites left behind.
const REFERRER: Address = Address::repeat_byte(0x7B);

const ETHER: u64 = 1_000_000_000_000_000_000;
const FEE: u64 = 3_700_000_000_000_000;

/// The suite writes block `n` at unix second `1_700_000_000 + n`: blocks
/// from `IN_A` fall on 2023-11-14 UTC, starting at `DAY_A`, and blocks from
/// `IN_B` on 2023-11-20 UTC, starting at `DAY_B`.
const IN_A: u64 = 10;
const IN_B: u64 = 500_000;
const DAY_A: i64 = 1_699_920_000;
const DAY_B: i64 = 1_700_438_400;

fn node(handle: &str) -> B256 {
    X.handle_node(handle).expect("an X handle")
}

/// The events of the suite's journey, as the contracts emit them.
trait Journey: Sized {
    /// A bind on X and the service fee its transaction paid.
    fn paid_bind(holder: Address, id: &str, handle: &str) -> Vec<Self>;
    /// A deposit to an X handle nobody holds, in round 0.
    fn deposited(handle: &str, token: Address, sender: Address, amount: u64) -> Self;
    /// The holder's claim of round 0 in one token.
    fn claimed(handle: &str, token: Address, claimer: Address, released: u64) -> Self;
}

impl Journey for NamesEvent {
    fn paid_bind(holder: Address, id: &str, handle: &str) -> Vec<Self> {
        let mut tx = Self::bind(holder, X, id, handle, 1, false);
        tx.push(Self::BindFeePaid {
            authorization_digest: node(handle),
            receiver: SAFE,
            amount: U256::from(FEE),
        });
        tx
    }

    fn deposited(handle: &str, token: Address, sender: Address, amount: u64) -> Self {
        Self::Escrow(EscrowEvent::Deposited {
            handle_node: node(handle),
            token,
            sender,
            round: U256::ZERO,
            amount: U256::from(amount),
            refundable_at: 1_800_000_000,
        })
    }

    fn claimed(handle: &str, token: Address, claimer: Address, released: u64) -> Self {
        Self::Escrow(EscrowEvent::Claimed {
            handle_node: node(handle),
            token,
            claimer,
            recipient: claimer,
            round: U256::ZERO,
            released: U256::from(released),
            received: U256::from(released),
        })
    }
}

fn links(platform: KnownPlatform, count: i64, holders: i64) -> PlatformLinks {
    PlatformLinks {
        platform: Some(platform),
        platform_id: platform.id(),
        count,
        holders,
    }
}

fn flow(token: Address, count: i64, amount: u128, wallets: i64) -> TokenFlow {
    TokenFlow {
        token,
        count,
        amount: U512::from(amount),
        wallets,
    }
}

/// `count` fees paid to the listed receiver, none to any other.
fn fees(count: i64) -> RegistrationFees {
    RegistrationFees {
        receivers: vec![FeesPaid {
            receiver: SAFE,
            count,
            amount: U512::from(FEE) * U512::from(count),
        }],
        other: None,
    }
}

/// The suite's lists: USDC and the fee Safe on its chain.
fn listing() -> Listing {
    let on_chain = |address| {
        vec![ListedAddress {
            chain_id: CHAIN,
            address,
        }]
    };
    Listing {
        tokens: on_chain(USDC),
        fee_receivers: on_chain(SAFE),
    }
}

/// Per-platform rows in the order the answer lists them: by platform id.
trait ByPlatform {
    fn by_platform(self) -> Self;
}

impl ByPlatform for Vec<PlatformLinks> {
    fn by_platform(mut self) -> Self {
        self.sort_by_key(|links| links.platform_id);
        self
    }
}

impl ByPlatform for Vec<PlatformHandles> {
    fn by_platform(mut self) -> Self {
        self.sort_by_key(|handles| handles.platform_id);
        self
    }
}

/// Flows of named tokens only, none under `other`.
trait Named {
    fn named(self) -> TokenFlows;
}

impl Named for Vec<TokenFlow> {
    fn named(self) -> TokenFlows {
        TokenFlows {
            tokens: self,
            other: None,
        }
    }
}

/// One chain's entry in an answer.
trait ChainEntry {
    fn chain(self, chain_id: i64) -> ChainStats;
}

impl ChainEntry for Stats {
    fn chain(self, chain_id: i64) -> ChainStats {
        self.chains
            .into_iter()
            .find(|chain| chain.chain_id == chain_id)
            .unwrap_or_else(|| panic!("chain {chain_id} listed"))
    }
}

/// One day of an answer across every chain; nothing counted when the day
/// is absent.
trait AllChainsEntry {
    fn on(&self, day: i64) -> AllChainsDay;
}

impl AllChainsEntry for [AllChainsDay] {
    fn on(&self, day: i64) -> AllChainsDay {
        self.iter()
            .find(|entry| entry.day == day)
            .cloned()
            .unwrap_or(AllChainsDay {
                day,
                users: 0,
                new_users: 0,
            })
    }
}

/// A running API: its cache lives as long as it does.
trait StatsRoute {
    /// `GET /v1/stats`.
    async fn stats(&self) -> Stats;
    /// Every page of `GET /v1/stats/days`, on `chain` or across every chain
    /// without it, oldest first.
    async fn days<D: DeserializeOwned>(&self, chain: Option<i64>) -> Vec<D>;
    /// One GET answered 200, in its type.
    async fn answer<T: DeserializeOwned>(&self, path: &str) -> T;
}

impl StatsRoute for Router {
    async fn stats(&self) -> Stats {
        self.answer("/v1/stats").await
    }

    async fn days<D: DeserializeOwned>(&self, chain: Option<i64>) -> Vec<D> {
        let on = chain
            .map(|chain| format!("&chain={chain}"))
            .unwrap_or_default();
        let mut days = Vec::new();
        let mut before = String::new();
        loop {
            let path = format!("/v1/stats/days?limit=100{on}{before}");
            let page: StatsDays<D> = self.answer(&path).await;
            assert_eq!(page.chain_id, chain);
            days.extend(page.days);
            match page.next {
                Some(next) => before = format!("&before={next}"),
                None => break,
            }
        }
        days.reverse();
        days
    }

    async fn answer<T: DeserializeOwned>(&self, path: &str) -> T {
        let request = Request::get(path).body(Body::empty()).unwrap();
        let response = self.clone().oneshot(request).await.expect("request");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body = response.into_body().collect().await.expect("body");
        serde_json::from_slice(&body.to_bytes()).expect("the answer's shape")
    }
}

/// Where each entry of a chain history sits: block and log index.
trait Positions {
    fn positions(&self) -> Vec<(i64, i64)>;
}

impl Positions for ChainHistory {
    fn positions(&self) -> Vec<(i64, i64)> {
        self.entries
            .iter()
            .map(|entry| (entry.block_number, entry.log_index))
            .collect()
    }
}

/// A suite's API.
trait Serves {
    /// An API over the suite's database, with its own cache.
    fn api(&self) -> Router;
}

impl Serves for Suite {
    fn api(&self) -> Router {
        AppState::new(Store::new(self.pool.clone())).router()
    }
}

/// Users, links, live handles, fees and every escrow movement add up per
/// chain, over the whole journal and per UTC day, each listed token by
/// name. A deposit to a handle nobody held, claimed by the holder who bound
/// it after, is an onboarding claim; a claim of deposits made before the
/// index's first block is not. A journal written before the rollups adds up
/// the same once its indexer folds it, also when the fold stopped after its
/// first page and a restart folded the rest.
#[tokio::test]
async fn the_journal_adds_up_per_chain_and_day() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        return;
    };
    // Day A: a paid link and a free one, then three deposits for dave, who has none.
    suite
        .apply_tx(IN_A, NamesEvent::paid_bind(ALICE, "1", "alice"))
        .await;
    suite
        .apply_tx(IN_A + 1, NamesEvent::bind(BOB, X, "2", "bob", 1, false))
        .await;
    for (block, token, sender, amount) in [
        (IN_A + 2, NATIVE, SENDER, ETHER),
        (IN_A + 3, NATIVE, PAYER, 2 * ETHER),
        (IN_A + 4, USDC, SENDER, 1_000),
    ] {
        let deposit = NamesEvent::deposited("dave", token, sender, amount);
        suite.apply(block, deposit).await;
    }
    // Day B: dave links and claims the coin, a send to alice is forwarded,
    // the token deposit is refunded, alice links GitHub and bob's handle is
    // retired; eve links and claims what came before the index started.
    suite
        .apply_tx(IN_B, NamesEvent::paid_bind(DAVE, "4", "dave"))
        .await;
    suite
        .apply(
            IN_B + 1,
            NamesEvent::claimed("dave", NATIVE, DAVE, 3 * ETHER),
        )
        .await;
    let forward = NamesEvent::Escrow(EscrowEvent::Forwarded {
        handle_node: node("alice"),
        token: NATIVE,
        sender: SENDER,
        holder: ALICE,
        amount: U256::from(5u64),
        received: U256::from(5u64),
    });
    suite.apply(IN_B + 2, forward).await;
    let refund = NamesEvent::Escrow(EscrowEvent::Refunded {
        handle_node: node("dave"),
        token: USDC,
        sender: SENDER,
        recipient: SENDER,
        round: U256::ZERO,
        released: U256::from(1_000u64),
        received: U256::from(1_000u64),
    });
    suite.apply(IN_B + 3, refund).await;
    let mut tx = NamesEvent::bind(ALICE, GITHUB, "3", "carol", 1, false);
    tx.push(NamesEvent::retire(X, "bob", BOB));
    suite.apply_tx(IN_B + 4, tx).await;
    suite
        .apply_tx(IN_B + 5, NamesEvent::bind(EVE, X, "5", "eve", 1, false))
        .await;
    suite
        .apply(IN_B + 6, NamesEvent::claimed("eve", NATIVE, EVE, 7))
        .await;

    let api = AppState::new(Store::new(suite.pool.clone()))
        .listing(listing())
        .router();
    let stats = api.stats().await;
    let days: Vec<DayActivity> = api.days(Some(CHAIN)).await;
    let ether = u128::from(ETHER);
    let handles = vec![
        PlatformHandles {
            platform: Some(X),
            platform_id: X.id(),
            live: 3,
            holders: 3,
        },
        PlatformHandles {
            platform: Some(GITHUB),
            platform_id: GITHUB.id(),
            live: 1,
            holders: 1,
        },
    ];
    let expected = ChainStats {
        chain_id: CHAIN,
        handles: handles.by_platform(),
        total: Activity {
            users: 6,
            links: vec![links(X, 4, 4), links(GITHUB, 1, 1)].by_platform(),
            registration_fees: fees(2),
            escrow: EscrowActivity {
                deposits: vec![flow(USDC, 1, 1_000, 1), flow(NATIVE, 2, 3 * ether, 2)]
                    .named(),
                forwards: vec![flow(NATIVE, 1, 5, 1)].named(),
                claims: vec![flow(NATIVE, 2, 3 * ether + 7, 2)].named(),
                refunds: vec![flow(USDC, 1, 1_000, 1)].named(),
            },
            onboarding_claims: vec![flow(NATIVE, 1, 3 * ether, 1)].named(),
        },
    };
    assert_eq!(stats.chain(CHAIN), expected);
    assert_eq!(
        days,
        [
            DayActivity {
                day: DAY_A,
                new_users: 4,
                activity: Activity {
                    users: 4,
                    links: vec![links(X, 2, 2)],
                    registration_fees: fees(1),
                    escrow: EscrowActivity {
                        deposits: vec![
                            flow(USDC, 1, 1_000, 1),
                            flow(NATIVE, 2, 3 * ether, 2),
                        ]
                        .named(),
                        ..EscrowActivity::default()
                    },
                    onboarding_claims: TokenFlows::default(),
                },
            },
            DayActivity {
                day: DAY_B,
                new_users: 2,
                activity: Activity {
                    users: 4,
                    links: vec![links(X, 2, 2), links(GITHUB, 1, 1)].by_platform(),
                    registration_fees: fees(1),
                    escrow: EscrowActivity {
                        deposits: TokenFlows::default(),
                        forwards: vec![flow(NATIVE, 1, 5, 1)].named(),
                        claims: vec![flow(NATIVE, 2, 3 * ether + 7, 2)].named(),
                        refunds: vec![flow(USDC, 1, 1_000, 1)].named(),
                    },
                    onboarding_claims: vec![flow(NATIVE, 1, 3 * ether, 1)].named(),
                },
            },
        ]
    );

    let folded = || {
        AppState::new(Store::new(suite.pool.clone()))
            .listing(listing())
            .router()
    };
    suite.store().unfold().await;
    suite.store().fold_one_page(5).await;
    let one_page = folded().stats().await.chain(CHAIN);
    assert_eq!(
        (one_page.total.users, one_page.total.escrow.deposits),
        (4, vec![flow(NATIVE, 2, 3 * ether, 2)].named()),
        "five rows: alice's paid bind, bob's bind and two coin deposits"
    );
    suite.store().writer().await;
    let restarted = folded();
    assert_eq!(restarted.stats().await.chain(CHAIN), expected);
    let days_restarted: Vec<DayActivity> = restarted.days(Some(CHAIN)).await;
    assert_eq!(days_restarted, days);
    suite.store().forget().await;
    suite.done();
}

/// A handle bound once, freed and then sent several deposits is an
/// onboarding when its next holder claims them: the bind before the first
/// deposit counts for nothing, the one after it brought the holder in.
#[tokio::test]
async fn a_freed_handle_onboards_its_next_holder() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        return;
    };
    // Day A: frank links "frank", then moves his account to another handle,
    // which frees it; two deposits follow.
    suite
        .apply_tx(IN_A, NamesEvent::bind(FRANK, X, "7", "frank", 1, false))
        .await;
    let mut moved = NamesEvent::bind(FRANK, X, "7", "frankie", 1, false);
    moved.push(NamesEvent::retire(X, "frank", FRANK));
    suite.apply_tx(IN_A + 1, moved).await;
    suite
        .apply(
            IN_A + 2,
            NamesEvent::deposited("frank", NATIVE, SENDER, ETHER),
        )
        .await;
    suite
        .apply(
            IN_A + 3,
            NamesEvent::deposited("frank", NATIVE, PAYER, 2 * ETHER),
        )
        .await;
    // Day B: grace's account takes the handle and claims both.
    suite
        .apply_tx(IN_B, NamesEvent::bind(GRACE, X, "8", "frank", 1, false))
        .await;
    suite
        .apply(
            IN_B + 1,
            NamesEvent::claimed("frank", NATIVE, GRACE, 3 * ETHER),
        )
        .await;

    let api = suite.api();
    let chain = api.stats().await.chain(CHAIN);
    let days: Vec<DayActivity> = api.days(Some(CHAIN)).await;
    let grace = || vec![flow(NATIVE, 1, 3 * u128::from(ETHER), 1)].named();
    assert_eq!(chain.total.onboarding_claims, grace());
    let onboarding_on = |day: i64| {
        days.iter()
            .find(|entry| entry.day == day)
            .map(|entry| entry.activity.onboarding_claims.clone())
            .unwrap_or_else(|| panic!("day {day} listed"))
    };
    assert_eq!(onboarding_on(DAY_A), TokenFlows::default());
    assert_eq!(onboarding_on(DAY_B), grace());
    suite.store().forget().await;
    suite.done();
}

/// Anybody can deposit a token they wrote. A thousand of them, and a token
/// the deployment did not list, count together under one `other` per kind
/// and day, without an amount, so the answer does not grow with them.
#[tokio::test]
async fn unlisted_tokens_count_together() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        return;
    };
    let junk = |tokens: Range<u64>| -> Vec<NamesEvent> {
        tokens
            .map(|i| Address::from_word(U256::from(i + 1).into()))
            .map(|token| NamesEvent::deposited("dave", token, SENDER, 1))
            .collect()
    };
    let mut day_a = junk(0..500);
    day_a.push(NamesEvent::deposited("dave", USDC, PAYER, 1_000));
    day_a.push(NamesEvent::deposited("dave", NATIVE, PAYER, ETHER));
    suite.apply_tx(IN_A, day_a).await;
    suite.apply_tx(IN_B, junk(500..1_000)).await;

    let api = suite.api();
    let chain = api.stats().await.chain(CHAIN);
    let days: Vec<DayActivity> = api.days(Some(CHAIN)).await;
    let deposits_on = |day: i64| {
        days.iter()
            .find(|entry| entry.day == day)
            .map(|entry| entry.activity.escrow.deposits.clone())
            .unwrap_or_else(|| panic!("day {day} listed"))
    };
    let coin = || vec![flow(NATIVE, 1, u128::from(ETHER), 1)];
    let other = |count| Some(OtherTokens { count });
    assert_eq!(
        chain.total.escrow.deposits,
        TokenFlows {
            tokens: coin(),
            other: other(1_001),
        }
    );
    assert_eq!(
        deposits_on(DAY_A),
        TokenFlows {
            tokens: coin(),
            other: other(501),
        }
    );
    assert_eq!(
        deposits_on(DAY_B),
        TokenFlows {
            tokens: Vec::new(),
            other: other(500),
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// A bind may name any fee receiver. A thousand of them count together
/// under one `other` per day and in total, with their amount, beside the
/// receiver the deployment listed, so the answer does not grow with them.
#[tokio::test]
async fn unlisted_fee_receivers_count_together() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let paid = |receivers: Range<u64>| -> Vec<NamesEvent> {
        receivers
            .map(|i| NamesEvent::BindFeePaid {
                authorization_digest: U256::from(i).into(),
                receiver: Address::from_word(U256::from(i + 1).into()),
                amount: U256::from(1u64),
            })
            .collect()
    };
    suite
        .apply_tx(IN_A, NamesEvent::paid_bind(ALICE, "1", "alice"))
        .await;
    suite.apply_tx(IN_A + 1, paid(0..500)).await;
    suite.apply_tx(IN_B, paid(500..1_000)).await;

    let api = AppState::new(Store::new(suite.pool.clone()))
        .listing(listing())
        .router();
    let chain = api.stats().await.chain(CHAIN);
    let days: Vec<DayActivity> = api.days(Some(CHAIN)).await;
    let fees_on = |day: i64| {
        days.iter()
            .find(|entry| entry.day == day)
            .map(|entry| entry.activity.registration_fees.clone())
            .unwrap_or_else(|| panic!("day {day} listed"))
    };
    let other = |count: i64| {
        Some(OtherFees {
            count,
            amount: U512::from(count),
        })
    };
    assert_eq!(
        chain.total.registration_fees,
        RegistrationFees {
            other: other(1_000),
            ..fees(1)
        }
    );
    assert_eq!(
        fees_on(DAY_A),
        RegistrationFees {
            other: other(500),
            ..fees(1)
        }
    );
    assert_eq!(
        fees_on(DAY_B),
        RegistrationFees {
            receivers: Vec::new(),
            other: other(500),
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// A wallet that used two chains counts once across them: in the total, and
/// as new only on the first day it used either, also when the chain that
/// saw it first is folded last. Each chain still counts it as new on its
/// own first day there. Forgetting the chain that saw both first leaves
/// them counted from the other chain's journal, new on its day. The numbers
/// across chains span every chain the database holds, so the test counts
/// what its writes add.
#[tokio::test]
async fn a_wallet_on_two_chains_counts_once_across_them() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        return;
    };
    let other = ChainStore::new(suite.pool.clone(), OTHER_CHAIN);
    other.forget().await;
    let other = other.writer().await;
    let then = suite.api();
    let before = then.stats().await;
    let days_before: Vec<AllChainsDay> = then.days(None).await;

    // Roamer is folded in day order, nomad's later day first.
    other
        .apply_tx(IN_A, NamesEvent::bind(ROAMER, X, "6", "roamer", 1, false))
        .await;
    suite
        .apply_tx(IN_B, NamesEvent::bind(ROAMER, X, "6", "roamer", 1, false))
        .await;
    suite
        .apply_tx(IN_B + 1, NamesEvent::bind(NOMAD, X, "9", "nomad", 1, false))
        .await;
    other
        .apply_tx(IN_A + 1, NamesEvent::bind(NOMAD, X, "9", "nomad", 1, false))
        .await;
    let now = suite.api();
    let after = now.stats().await;
    let days_after: Vec<AllChainsDay> = now.days(None).await;

    assert_eq!(after.total.users - before.total.users, 2);
    let added = |days: &[AllChainsDay], day: i64| {
        let (added, had) = (days.on(day), days_before.on(day));
        AllChainsDay {
            day,
            users: added.users - had.users,
            new_users: added.new_users - had.new_users,
        }
    };
    assert_eq!(
        added(&days_after, DAY_A),
        AllChainsDay {
            day: DAY_A,
            users: 2,
            new_users: 2,
        }
    );
    assert_eq!(
        added(&days_after, DAY_B),
        AllChainsDay {
            day: DAY_B,
            users: 2,
            new_users: 0,
        }
    );
    let chain = after.chain(CHAIN);
    assert_eq!(chain.total.users, 2);
    let chain_days: Vec<DayActivity> = now.days(Some(CHAIN)).await;
    let day_b = chain_days
        .iter()
        .find(|entry| entry.day == DAY_B)
        .expect("day B listed");
    assert_eq!(day_b.new_users, 2);

    ChainStore::new(suite.pool.clone(), OTHER_CHAIN)
        .forget()
        .await;
    let cleared = suite.api();
    assert_eq!(cleared.stats().await.total.users - before.total.users, 2);
    let days_cleared: Vec<AllChainsDay> = cleared.days(None).await;
    assert_eq!(
        [added(&days_cleared, DAY_A), added(&days_cleared, DAY_B)],
        [
            AllChainsDay {
                day: DAY_A,
                users: 0,
                new_users: 0,
            },
            AllChainsDay {
                day: DAY_B,
                users: 2,
                new_users: 2,
            },
        ]
    );
    suite.store().forget().await;
    suite.done();
}

/// Two chains' windows that bring the same new wallets fold at once: the
/// second waits on the rows the first wrote across chains, then both
/// commit, and each wallet counts once across chains, new on the first
/// chain's day. The numbers across chains span every chain the database
/// holds, so the test counts what its writes add.
#[tokio::test]
async fn two_chains_fold_the_same_new_wallets_at_once() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // Each chain writes on sessions of its own name, so a wait below is that
    // chain's wait and no other session's.
    const FIRST: &str = "stats: the first chain's window";
    const SECOND: &str = "stats: the second chain's window";
    let named = |name: &str| {
        PgPool::connect_with(
            (*suite.pool.connect_options())
                .clone()
                .application_name(name),
        )
    };
    let first = ChainStore::new(named(FIRST).await.expect("a pool"), CHAIN)
        .writer()
        .await;
    let other = ChainStore::new(named(SECOND).await.expect("a pool"), OTHER_CHAIN);
    other.forget().await;
    let second = other.writer().await;
    first
        .apply_tx(IN_A, NamesEvent::bind(ALICE, X, "1", "alice", 1, false))
        .await;
    let then = suite.api();
    let before = then.stats().await;
    let days_before: Vec<AllChainsDay> = then.days(None).await;
    let both = || {
        let mut binds = NamesEvent::bind(ROAMER, X, "6", "roamer", 1, false);
        binds.extend(NamesEvent::bind(NOMAD, X, "9", "nomad", 1, false));
        binds
    };

    // The first window stops at the chain's users total, which the test
    // holds, after it wrote the wallets across chains; the second waits on
    // those.
    let mut hold = suite.pool.begin().await.expect("a transaction");
    sqlx::query(include_str!("sql/hold_chain_users.sql"))
        .bind(CHAIN)
        .fetch_one(&mut *hold)
        .await
        .expect("the chain's users total is held");
    let first = tokio::spawn(async move { first.apply_tx(IN_B, both()).await });
    suite.pool.waits_on_a_lock(FIRST).await;
    let second = tokio::spawn(async move { second.apply_tx(IN_B, both()).await });
    suite.pool.waits_on_a_lock(SECOND).await;
    hold.rollback().await.expect("the hold ends");
    first.await.expect("the first window commits");
    second.await.expect("the second window commits");

    let now = suite.api();
    let after = now.stats().await;
    let days_after: Vec<AllChainsDay> = now.days(None).await;
    assert_eq!(after.total.users - before.total.users, 2);
    let (added, had) = (days_after.on(DAY_B), days_before.on(DAY_B));
    assert_eq!(
        AllChainsDay {
            day: DAY_B,
            users: added.users - had.users,
            new_users: added.new_users - had.new_users,
        },
        AllChainsDay {
            day: DAY_B,
            users: 2,
            new_users: 2,
        }
    );
    assert_eq!(after.clone().chain(CHAIN).total.users, 3);
    assert_eq!(after.chain(OTHER_CHAIN).total.users, 2);
    ChainStore::new(suite.pool.clone(), OTHER_CHAIN)
        .forget()
        .await;
    suite.store().forget().await;
    suite.done();
}

/// A page of days holds the last 30 unless asked for more, newest first,
/// and hands out where the next one starts while older days remain.
#[tokio::test]
async fn days_come_thirty_to_a_page() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // One bind a day for 31 days from day A, each by its own account.
    let starts: Vec<i64> = (0..31).map(|day| DAY_A + day * 86_400).collect();
    for day in 0..31u64 {
        let (id, handle) = ((100 + day).to_string(), format!("alice{day}"));
        let bind = NamesEvent::bind(ALICE, X, &id, &handle, 1, false);
        suite.apply_tx(IN_A + day * 86_400, bind).await;
    }

    let api = suite.api();
    let first: StatsDays<DayActivity> =
        api.answer(&format!("/v1/stats/days?chain={CHAIN}")).await;
    let served = |page: &StatsDays<DayActivity>| -> Vec<i64> {
        page.days.iter().map(|entry| entry.day).collect()
    };
    let newest: Vec<i64> = starts.iter().rev().take(30).copied().collect();
    assert_eq!(served(&first), newest);
    let next = first.next.clone().expect("an older day remains");
    let last: StatsDays<DayActivity> = api
        .answer(&format!("/v1/stats/days?chain={CHAIN}&before={next}"))
        .await;
    assert_eq!(served(&last), [DAY_A]);
    assert_eq!(last.next, None);
    suite.store().forget().await;
    suite.done();
}

/// The referral splitter sends every paid share through the escrow: its
/// deposits and forwards count as such, and it counts as no user and among
/// no movement's wallets. A referral and a bind fee's setting count nowhere:
/// the bind's own fee counts once, as `BindFeePaid`.
#[tokio::test]
async fn the_referral_splitter_is_no_user() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        return;
    };
    let before = suite.api().stats().await;
    suite
        .apply_tx(
            IN_A,
            NamesEvent::bind(REFERRER, X, "9", "referrer", 1, false),
        )
        .await;
    let forward = NamesEvent::Escrow(EscrowEvent::Forwarded {
        handle_node: node("referrer"),
        token: NATIVE,
        sender: SPLITTER,
        holder: REFERRER,
        amount: U256::from(5u64),
        received: U256::from(5u64),
    });
    suite.apply(IN_A + 1, forward).await;
    let held = NamesEvent::deposited("dave", NATIVE, SPLITTER, 7);
    suite.apply(IN_A + 2, held).await;
    let own = NamesEvent::Referred {
        referrer_node: node("referrer"),
        binder: REFERRER,
        session_id: B256::repeat_byte(0x5E),
        referrer_platform_id: X.id(),
        platform_id: X.id(),
        handle_node: node("referrer"),
        fee: U256::from(FEE),
        share: U256::from(3u64),
        outcome: Outcome::SelfReferral,
    };
    suite.apply(IN_B, own).await;
    let price = NamesEvent::BindFeeSet {
        fee: U256::from(FEE),
    };
    suite.apply(IN_B + 1, price).await;
    let api = suite.api();
    let after = api.stats().await;
    let days: Vec<DayActivity> = api.days(Some(CHAIN)).await;

    assert_eq!(after.total.users - before.total.users, 1);
    let chain = after.chain(CHAIN);
    assert_eq!(chain.total.users, 1);
    assert_eq!(
        chain.total.escrow.deposits,
        vec![flow(NATIVE, 1, 7, 0)].named()
    );
    assert_eq!(
        chain.total.escrow.forwards,
        vec![flow(NATIVE, 1, 5, 0)].named()
    );
    let [day] = days.as_slice() else {
        panic!("one day listed, not {days:?}");
    };
    assert_eq!((day.day, day.activity.users, day.new_users), (DAY_A, 1, 1));
    suite.store().forget().await;
    suite.done();
}

/// A running API serves its answer again, to requests arriving together
/// too, while one started after the journal changed counts the change.
#[tokio::test]
async fn an_answer_is_served_again_until_it_expires() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        return;
    };
    suite
        .apply_tx(IN_A, NamesEvent::bind(ALICE, X, "1", "alice", 1, false))
        .await;
    let api = suite.api();
    let first = api.stats().await;
    assert_eq!(first.clone().chain(CHAIN).total.links, [links(X, 1, 1)]);

    suite
        .apply_tx(IN_A + 1, NamesEvent::bind(BOB, X, "2", "bob", 1, false))
        .await;
    let mut requests = JoinSet::new();
    for _ in 0..10 {
        let api = api.clone();
        requests.spawn(async move { api.stats().await });
    }
    while let Some(served) = requests.join_next().await {
        assert_eq!(served.expect("a request"), first);
    }

    let restarted: Stats = suite.get("/v1/stats").await.answer();
    assert_eq!(restarted.chain(CHAIN).total.links, [links(X, 2, 2)]);
    suite.store().forget().await;
    suite.done();
}

/// A day's events come newest first, a page at a time, narrowed to a kind, a
/// token or both; the journal's other days and a day with nothing stay out.
#[tokio::test]
async fn a_day_lists_its_events_page_by_page() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let mut day_a = Vec::new();
    let bind = NamesEvent::paid_bind(ALICE, "1", "alice");
    day_a.extend((0..bind.len()).map(|log| (IN_A, log)));
    suite.apply_tx(IN_A, bind).await;
    for (block, token) in [(IN_A + 1, NATIVE), (IN_A + 2, USDC)] {
        suite
            .apply(block, NamesEvent::deposited("dave", token, SENDER, 1_000))
            .await;
        day_a.push((block, 0));
    }
    suite
        .apply_tx(IN_B, NamesEvent::paid_bind(DAVE, "4", "dave"))
        .await;
    suite
        .apply(IN_B + 1, NamesEvent::claimed("dave", NATIVE, DAVE, 1_000))
        .await;
    let newest_first = |positions: &[(u64, usize)]| -> Vec<(i64, i64)> {
        positions
            .iter()
            .rev()
            .map(|&(block, log)| (block as i64, log as i64))
            .collect()
    };

    let api = suite.api();
    let history = |query: String| {
        let api = api.clone();
        async move {
            let page: ChainHistory = api
                .answer(&format!("/v1/history/chain/{CHAIN}?{query}"))
                .await;
            page
        }
    };
    let whole = history(format!("day={DAY_A}&limit=100")).await;
    assert_eq!(whole.chain_id, CHAIN);
    assert_eq!(whole.day, Some(DAY_A));
    assert_eq!(whole.positions(), newest_first(&day_a));
    assert_eq!(whole.next, None);

    let mut paged = Vec::new();
    let mut before = String::new();
    loop {
        let page = history(format!("day={DAY_A}&limit=2{before}")).await;
        assert!(page.entries.len() <= 2);
        paged.extend(page.positions());
        match page.next {
            Some(next) => before = format!("&before={next}"),
            None => break,
        }
    }
    assert_eq!(paged, whole.positions());

    let deposits = history(format!("day={DAY_A}&kind=deposited")).await;
    assert_eq!(
        deposits.positions(),
        newest_first(&[(IN_A + 1, 0), (IN_A + 2, 0)])
    );
    let usdc = history(format!("token={USDC}")).await;
    assert_eq!(usdc.positions(), newest_first(&[(IN_A + 2, 0)]));
    let claims = history(format!("day={DAY_B}&kind=claimed&token={NATIVE}")).await;
    assert_eq!(claims.positions(), newest_first(&[(IN_B + 1, 0)]));
    let quiet = history(format!("day={}", DAY_A + 86_400)).await;
    assert_eq!(quiet.positions(), []);
    assert_eq!(quiet.next, None);
    suite.store().forget().await;
    suite.done();
}
