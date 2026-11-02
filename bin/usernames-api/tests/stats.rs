//! `GET /v1/stats` over a journal written through the indexer's apply path:
//! users, links, fees and escrow movements counted per chain, in total and
//! per UTC day, and one answer served until it expires.
//!
//! Skips without `DATABASE_URL`, unless `CI` is set: there its absence
//! fails. The suite's chain is its own; the answer lists every chain the
//! database holds, so it reads only its own.

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
use tokio::{
    sync::Mutex,
    task::JoinSet,
};
use tower::ServiceExt;
use usernames_core::{
    api::{
        model::{
            Activity,
            ChainStats,
            DayActivity,
            EscrowActivity,
            FeesPaid,
            PlatformHandles,
            PlatformLinks,
            Stats,
            TokenFlow,
        },
        AppState,
    },
    db::Store,
    events::{
        EscrowEvent,
        NamesEvent,
    },
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    Events,
    Suite,
};

const CHAIN: i64 = 31351;

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
const SENDER: Address = Address::repeat_byte(0x51);
const PAYER: Address = Address::repeat_byte(0x52);
const SAFE: Address = Address::repeat_byte(0xFE);

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

fn fees(count: i64) -> Vec<FeesPaid> {
    vec![FeesPaid {
        receiver: SAFE,
        count,
        amount: U512::from(FEE) * U512::from(count),
    }]
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

/// A running API: its cache lives as long as it does.
trait StatsRoute {
    /// `GET /v1/stats`.
    async fn stats(&self) -> Stats;
}

impl StatsRoute for Router {
    async fn stats(&self) -> Stats {
        let request = Request::get("/v1/stats").body(Body::empty()).unwrap();
        let response = self.clone().oneshot(request).await.expect("request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.expect("body");
        serde_json::from_slice(&body.to_bytes()).expect("the stats' shape")
    }
}

/// Users, links, live handles, fees and every escrow movement add up per
/// chain, over the whole journal and per UTC day. A deposit to a handle nobody held,
/// claimed by the holder who bound it after, is an onboarding claim; a
/// claim of deposits made before the index's first block is not.
#[tokio::test]
async fn the_journal_adds_up_per_chain_and_day() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
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

    let stats: Stats = suite.get("/v1/stats").await.answer();
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
                deposits: vec![flow(USDC, 1, 1_000, 1), flow(NATIVE, 2, 3 * ether, 2)],
                forwards: vec![flow(NATIVE, 1, 5, 1)],
                claims: vec![flow(NATIVE, 2, 3 * ether + 7, 2)],
                refunds: vec![flow(USDC, 1, 1_000, 1)],
            },
            onboarding_claims: vec![flow(NATIVE, 1, 3 * ether, 1)],
        },
        days: vec![
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
                        ],
                        ..EscrowActivity::default()
                    },
                    onboarding_claims: Vec::new(),
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
                        deposits: Vec::new(),
                        forwards: vec![flow(NATIVE, 1, 5, 1)],
                        claims: vec![flow(NATIVE, 2, 3 * ether + 7, 2)],
                        refunds: vec![flow(USDC, 1, 1_000, 1)],
                    },
                    onboarding_claims: vec![flow(NATIVE, 1, 3 * ether, 1)],
                },
            },
        ],
    };
    assert_eq!(stats.chain(CHAIN), expected);
    suite.done();
}

/// A running API serves its answer again, to requests arriving together
/// too, while one started after the journal changed counts the change.
#[tokio::test]
async fn an_answer_is_served_again_until_it_expires() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    suite
        .apply_tx(IN_A, NamesEvent::bind(ALICE, X, "1", "alice", 1, false))
        .await;
    let api = AppState::new(Store::new(suite.pool.clone())).router();
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
    suite.done();
}
