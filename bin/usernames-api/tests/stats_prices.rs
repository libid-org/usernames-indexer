//! `GET /v1/stats` valued in USD, against a price source on this machine
//! that answers as DefiLlama does, `prices/current/<coins>` and
//! `v2/chains`: a token priced with confidence is named and valued, every
//! other one counts under `other`, each day valued at the same prices, a
//! failed request keeps the last prices and chain list, a chain the list
//! leaves out is not priced, a token priced once keeps its place among
//! the coins asked, and escrow amounts are valued at the prices served, or
//! answer unpriced when the computation fails.
//!
//! Runs against the Postgres at `DATABASE_URL`, and fails without it.
//! The suite's chain is its own, listed by the source as
//! `TestChain`; each test forgets it when it ends.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        Mutex as StdMutex,
    },
    time::{
        Duration,
        Instant,
        SystemTime,
        UNIX_EPOCH,
    },
};

use alloy::primitives::{
    address,
    Address,
    U256,
    U512,
};
use axum::{
    extract::{
        Path,
        State,
    },
    http::StatusCode,
    routing::get,
    Json,
    Router,
};
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::Mutex,
    task::JoinHandle,
};
use url::Url;
use usernames_core::{
    api::{
        model::{
            AddressAmounts,
            AddressTotals,
            Amount,
            ChainTotals,
            DayActivity,
            ErrorCode,
            HandleAmounts,
            OtherTokens,
            Stats,
            StatsDays,
            TokenFlow,
            TokenFlows,
            TokenTotals,
            TokenValue,
            Totals,
            Usd,
        },
        AppState,
        PriceSource,
        Prices,
        KEPT_PER_CHAIN,
        STATS_TTL,
    },
    db::{
        Store,
        CANDIDATES_PER_CHAIN,
    },
    events::{
        EscrowEvent,
        NamesEvent,
    },
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    ChainEntry,
    Events,
    StatsRoute,
    Suite,
};

const CHAIN: i64 = 31353;
/// The suite chain's entry in the source's chain list.
const LISTED_NAME: &str = "TestChain";
/// The suite chain's name in the source's coin ids: the listed name,
/// lowercased.
const CHAIN_NAME: &str = "testchain";

static SUITE: Mutex<()> = Mutex::const_new(());

/// The chain's own coin, as the escrow names it (EIP-7528).
const NATIVE: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
const USDC: Address = Address::repeat_byte(0x05);
/// Priced, and sorted between `USDC` and `JUNK`.
const DAI: Address = Address::repeat_byte(0x07);
const JUNK: Address = Address::repeat_byte(0x0B);

const SENDER: Address = Address::repeat_byte(0x51);
const PAYER: Address = Address::repeat_byte(0x52);
/// Owned by this suite alone: totals read every chain.
const HOLDER: Address = Address::repeat_byte(0xA5);

const ETHER: u64 = 1_000_000_000_000_000_000;
const DAY: i64 = 24 * 60 * 60;

/// Unix seconds now.
fn now() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970");
    i64::try_from(since.as_secs()).expect("a unix second")
}

/// A coin as the source answers it.
#[derive(Clone, Serialize)]
struct Coin {
    price: f64,
    decimals: u64,
    symbol: String,
    confidence: f64,
    timestamp: i64,
}

impl Coin {
    /// A coin priced a minute ago, with confidence.
    fn priced(price: f64, decimals: u64, symbol: &str) -> Self {
        Self {
            price,
            decimals,
            symbol: symbol.to_string(),
            confidence: 0.99,
            timestamp: now() - 60,
        }
    }
}

/// The source's answer.
#[derive(Serialize)]
struct Answer {
    coins: BTreeMap<String, Coin>,
}

/// An entry of the source's chain list.
#[derive(Clone, Serialize)]
struct ListedChain {
    #[serde(rename = "chainId")]
    chain_id: i64,
    name: String,
}

/// What the source prices and lists and whether it answers, and the coin
/// ids it was asked for.
#[derive(Default)]
struct Book {
    coins: BTreeMap<String, Coin>,
    chains: Vec<ListedChain>,
    failing: bool,
    chains_failing: bool,
    asked: Vec<String>,
}

/// The source's id of a token on the suite's chain.
fn coin_id(token: Address) -> String {
    let token = if token == NATIVE {
        Address::ZERO
    } else {
        token
    };
    format!("{CHAIN_NAME}:{token:#x}")
}

/// `GET /prices/current/<coins>`: the coins the book prices, or a 503
/// while it fails.
async fn answer(
    State(book): State<Arc<StdMutex<Book>>>,
    Path(coins): Path<String>,
) -> Result<Json<Answer>, StatusCode> {
    let mut book = book.lock().expect("the book");
    book.asked.extend(coins.split(',').map(str::to_string));
    let answer = (!book.failing).then(|| Answer {
        coins: coins
            .split(',')
            .filter_map(|id| Some((id.to_string(), book.coins.get(id)?.clone())))
            .collect(),
    });
    drop(book);
    answer.map(Json).ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// `GET /v2/chains`: the chains the book lists, or a 503 while the list
/// fails.
async fn chains(
    State(book): State<Arc<StdMutex<Book>>>,
) -> Result<Json<Vec<ListedChain>>, StatusCode> {
    let book = book.lock().expect("the book");
    let chains = (!book.chains_failing).then(|| book.chains.clone());
    drop(book);
    chains.map(Json).ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// A price source serving on a port of its own until dropped.
struct MockSource {
    base: Url,
    book: Arc<StdMutex<Book>>,
    server: JoinHandle<()>,
}

impl MockSource {
    /// A source listing the suite's chain.
    async fn start() -> Self {
        let book = Arc::new(StdMutex::new(Book {
            chains: vec![ListedChain {
                chain_id: CHAIN,
                name: LISTED_NAME.to_string(),
            }],
            ..Book::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("its address");
        let base = format!("http://{addr}").parse().expect("a URL");
        let app = Router::new()
            .route("/prices/current/{coins}", get(answer))
            .route("/v2/chains", get(chains))
            .with_state(Arc::clone(&book));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serving");
        });
        Self { base, book, server }
    }

    /// The source prices `token` at `coin` from now on.
    fn price(&self, token: Address, coin: Coin) {
        let mut book = self.book.lock().expect("the book");
        book.coins.insert(coin_id(token), coin);
        drop(book);
    }

    /// The source answers 503 from now on.
    fn fail(&self) {
        let mut book = self.book.lock().expect("the book");
        book.failing = true;
        drop(book);
    }

    /// The chain list answers 503 from now on.
    fn fail_chains(&self) {
        let mut book = self.book.lock().expect("the book");
        book.chains_failing = true;
        drop(book);
    }

    /// The chain list leaves the suite's chain out from now on.
    fn unlist(&self) {
        let mut book = self.book.lock().expect("the book");
        book.chains.clear();
        drop(book);
    }

    /// The coin ids asked since the last call.
    fn asked(&self) -> Vec<String> {
        let mut book = self.book.lock().expect("the book");
        let asked = std::mem::take(&mut book.asked);
        drop(book);
        asked
    }

    /// Prices from this source.
    fn source(&self) -> Arc<Prices> {
        Arc::new(Prices::mock(PriceSource::DefiLlama, &self.base))
    }
}

impl Drop for MockSource {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The escrow's movements, as the contract emits them.
trait Payment: Sized {
    /// A deposit of `amount` to a handle nobody holds, in round 0.
    fn deposited(token: Address, sender: Address, amount: U256) -> Self;
    /// A send of `amount` straight to the holder.
    fn forwarded(token: Address, sender: Address, amount: U256) -> Self;
}

impl Payment for NamesEvent {
    fn deposited(token: Address, sender: Address, amount: U256) -> Self {
        Self::Escrow(EscrowEvent::Deposited {
            handle_node: KnownPlatform::X.handle_node("dave").expect("a handle"),
            token,
            sender,
            round: U256::ZERO,
            amount,
            refundable_at: 1_800_000_000,
        })
    }

    fn forwarded(token: Address, sender: Address, amount: U256) -> Self {
        Self::Escrow(EscrowEvent::Forwarded {
            handle_node: KnownPlatform::X.handle_node("alice").expect("a handle"),
            token,
            sender,
            holder: HOLDER,
            amount,
            received: amount,
        })
    }
}

fn usd(text: &str) -> Usd {
    text.parse().expect("a USD amount")
}

/// One token's deposits or forwards, one movement by one wallet.
trait Flow {
    /// `amount` of `token`, without a price.
    fn unpriced(token: Address, amount: U512) -> Self;
    /// The same flow valued as `symbol` at `decimals`: `value` after them,
    /// `usd` in USD.
    fn valued(self, symbol: (&str, u8), value: &str, usd: &str) -> Self;
}

impl Flow for TokenFlow {
    fn unpriced(token: Address, amount: U512) -> Self {
        Self {
            token,
            count: 1,
            amount,
            wallets: 1,
            priced: TokenValue::default(),
        }
    }

    fn valued(
        self,
        (symbol, decimals): (&str, u8),
        value: &str,
        usd_value: &str,
    ) -> Self {
        Self {
            priced: TokenValue::of((symbol, decimals), value, usd_value),
            ..self
        }
    }
}

/// A token amount as the source values it.
trait Valued {
    /// `symbol` at `decimals`: `value` after them, `usd` in USD.
    fn of(symbol: (&str, u8), value: &str, usd: &str) -> Self;
}

impl Valued for TokenValue {
    fn of((symbol, decimals): (&str, u8), value: &str, usd_value: &str) -> Self {
        Self {
            symbol: Some(symbol.to_string()),
            decimals: Some(decimals),
            value: Some(value.to_string()),
            usd: Some(usd(usd_value)),
        }
    }
}

/// An escrow answer's amounts, by token.
trait PricedAmounts {
    /// Each token's valuation.
    fn priced(&self) -> BTreeMap<Address, TokenValue>;
}

impl PricedAmounts for HandleAmounts {
    fn priced(&self) -> BTreeMap<Address, TokenValue> {
        self.amounts
            .iter()
            .map(|amount| (amount.token, amount.priced.clone()))
            .collect()
    }
}

impl PricedAmounts for AddressAmounts {
    fn priced(&self) -> BTreeMap<Address, TokenValue> {
        self.amounts
            .iter()
            .map(|amount| (amount.token, amount.priced.clone()))
            .collect()
    }
}

fn ether(tenths: u64) -> U256 {
    U256::from(ETHER / 10 * tenths)
}

/// A token the source prices with confidence is named, its amount after
/// its decimals and in USD, truncated to the cent; each list sums its
/// priced tokens, and the payments across chains sum deposits and
/// forwards. `pricedAt` is the oldest price used.
#[tokio::test]
async fn priced_tokens_are_valued_in_usd() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    let ether_price = Coin {
        timestamp: now() - 100,
        ..Coin::priced(2000.5, 18, "ETH")
    };
    let oldest = ether_price.timestamp;
    mock.price(NATIVE, ether_price);
    mock.price(USDC, Coin::priced(0.9998, 6, "USDC"));
    let api = || {
        AppState::new(Store::new(suite.pool.clone()))
            .pricing(mock.source())
            .router()
    };
    let before = api().stats().await;

    let deposits = vec![
        NamesEvent::deposited(NATIVE, SENDER, ether(15)),
        NamesEvent::deposited(USDC, PAYER, U256::from(1_234_567u64)),
        NamesEvent::deposited(JUNK, SENDER, U256::from(5u64)),
    ];
    suite.apply_tx(10, deposits).await;
    suite
        .apply(11, NamesEvent::forwarded(NATIVE, SENDER, ether(5)))
        .await;
    let after = api().stats().await;

    let chain = after.clone().chain(CHAIN);
    let usdc = ("USDC", 6);
    let eth = ("ETH", 18);
    assert_eq!(
        chain.total.escrow.deposits,
        TokenFlows {
            tokens: vec![
                TokenFlow::unpriced(USDC, U512::from(1_234_567u64))
                    .valued(usdc, "1.234567", "1.23"),
                TokenFlow::unpriced(NATIVE, ether(15).to()).valued(eth, "1.5", "3000.75"),
            ],
            usd: usd("3001.98"),
            other: Some(OtherTokens { count: 1 }),
        }
    );
    assert_eq!(
        chain.total.escrow.forwards,
        TokenFlows {
            tokens: vec![
                TokenFlow::unpriced(NATIVE, ether(5).to()).valued(eth, "0.5", "1000.25")
            ],
            usd: usd("1000.25"),
            other: None,
        }
    );
    assert_eq!(after.priced_at, Some(oldest));
    assert_eq!(before.total.payments.usd, Usd::default());
    assert_eq!(after.total.payments.usd, usd("4002.23"));
    assert_eq!(
        after.total.payments.unpriced - before.total.payments.unpriced,
        1
    );
    suite.store().forget().await;
    suite.done();
}

/// `GET /v1/stats/days` names and values each day's tokens as the
/// `/v1/stats` answer being served does, at its prices, and gives its
/// `pricedAt`; asked first, it computes that answer.
#[tokio::test]
async fn days_are_valued_at_the_answers_prices() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(NATIVE, Coin::priced(2000.5, 18, "ETH"));
    mock.price(USDC, Coin::priced(0.9998, 6, "USDC"));
    let api = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router();
    let deposits = vec![
        NamesEvent::deposited(NATIVE, SENDER, ether(15)),
        NamesEvent::deposited(USDC, PAYER, U256::from(1_234_567u64)),
    ];
    suite.apply_tx(10, deposits).await;

    let days: Vec<DayActivity> = api.days(Some(CHAIN)).await;
    let page: StatsDays<DayActivity> =
        api.answer(&format!("/v1/stats/days?chain={CHAIN}")).await;
    let stats = api.stats().await;
    let deposits = TokenFlows {
        tokens: vec![
            TokenFlow::unpriced(USDC, U512::from(1_234_567u64)).valued(
                ("USDC", 6),
                "1.234567",
                "1.23",
            ),
            TokenFlow::unpriced(NATIVE, ether(15).to()).valued(
                ("ETH", 18),
                "1.5",
                "3000.75",
            ),
        ],
        usd: usd("3001.98"),
        other: None,
    };
    assert_eq!(stats.clone().chain(CHAIN).total.escrow.deposits, deposits);
    let [day] = days.as_slice() else {
        panic!("one day recorded: {days:?}");
    };
    assert_eq!(day.activity.escrow.deposits, deposits);
    assert!(stats.priced_at.is_some());
    assert_eq!(page.priced_at, stats.priced_at);
    suite.store().forget().await;
    suite.done();
}

/// A token the source does not know, and one it prices with confidence
/// under 0.9, count under `other`; both were asked.
#[tokio::test]
async fn a_token_without_a_confident_price_counts_under_other() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(
        USDC,
        Coin {
            confidence: 0.5,
            ..Coin::priced(1.0, 6, "USDC")
        },
    );
    let deposits = vec![
        NamesEvent::deposited(USDC, PAYER, U256::from(1_000u64)),
        NamesEvent::deposited(JUNK, SENDER, U256::from(5u64)),
    ];
    suite.apply_tx(10, deposits).await;

    let stats = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router()
        .stats()
        .await;
    let mut asked = mock.asked();
    asked.sort();
    assert_eq!(asked, [coin_id(USDC), coin_id(JUNK)]);
    assert_eq!(
        stats.clone().chain(CHAIN).total.escrow.deposits,
        TokenFlows {
            tokens: Vec::new(),
            usd: Usd::default(),
            other: Some(OtherTokens { count: 2 }),
        }
    );
    assert_eq!(stats.priced_at, None);
    suite.store().forget().await;
    suite.done();
}

/// A request that fails keeps the prices the source gave before, and
/// `pricedAt` stays their time.
#[tokio::test]
async fn a_failed_request_keeps_the_last_prices() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    let ether_price = Coin::priced(2000.0, 18, "ETH");
    let priced_at = ether_price.timestamp;
    mock.price(NATIVE, ether_price);
    let source = mock.source();
    let api = || {
        AppState::new(Store::new(suite.pool.clone()))
            .pricing(Arc::clone(&source))
            .router()
    };
    suite
        .apply(10, NamesEvent::deposited(NATIVE, SENDER, ether(10)))
        .await;
    let first = api().stats().await;
    assert_eq!(first.chain(CHAIN).total.escrow.deposits.usd, usd("2000.00"));

    mock.fail();
    suite
        .apply(11, NamesEvent::deposited(NATIVE, PAYER, ether(10)))
        .await;
    let second = api().stats().await;
    assert_eq!(second.priced_at, Some(priced_at));
    assert_eq!(
        second.chain(CHAIN).total.escrow.deposits.usd,
        usd("4000.00")
    );
    suite.store().forget().await;
    suite.done();
}

/// With no price ever given, nothing is valued: the chain's coin is named
/// without a price, every other token counts under `other`, and the answer
/// is still served.
#[tokio::test]
async fn a_failed_first_request_prices_nothing() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(USDC, Coin::priced(1.0, 6, "USDC"));
    mock.fail();
    let deposits = vec![
        NamesEvent::deposited(NATIVE, SENDER, ether(10)),
        NamesEvent::deposited(USDC, PAYER, U256::from(1_000u64)),
    ];
    suite.apply_tx(10, deposits).await;

    let stats = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router()
        .stats()
        .await;
    assert_eq!(stats.priced_at, None);
    assert_eq!(
        stats.chain(CHAIN).total.escrow.deposits,
        TokenFlows {
            tokens: vec![TokenFlow::unpriced(NATIVE, ether(10).to())],
            usd: Usd::default(),
            other: Some(OtherTokens { count: 1 }),
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// An escrow amount of a priced token carries its symbol, decimals, value
/// and USD at the prices `/v1/stats` serves, and the answer their
/// `pricedAt`; any other token answers raw units.
#[tokio::test]
async fn escrow_amounts_are_valued_at_the_served_prices() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    let ether_price = Coin {
        timestamp: now() - 100,
        ..Coin::priced(2000.5, 18, "ETH")
    };
    let oldest = ether_price.timestamp;
    mock.price(NATIVE, ether_price);
    mock.price(USDC, Coin::priced(0.9998, 6, "USDC"));
    let deposits = vec![
        NamesEvent::deposited(NATIVE, SENDER, ether(15)),
        NamesEvent::deposited(USDC, PAYER, U256::from(1_234_567u64)),
        NamesEvent::deposited(JUNK, SENDER, U256::from(5u64)),
    ];
    suite.apply_tx(10, deposits).await;

    let api = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router();
    let held: HandleAmounts = api
        .answer(&format!("/v1/escrow/handle/x/dave?chain={CHAIN}"))
        .await;
    assert_eq!(held.priced_at, Some(oldest));
    assert_eq!(
        held.priced(),
        BTreeMap::from([
            (NATIVE, TokenValue::of(("ETH", 18), "1.5", "3000.75")),
            (USDC, TokenValue::of(("USDC", 6), "1.234567", "1.23")),
            (JUNK, TokenValue::default()),
        ])
    );
    // Bound, the same slots are the holder's claim, valued the same way.
    suite
        .apply_tx(
            11,
            NamesEvent::bind(HOLDER, KnownPlatform::X, "901", "dave", 5_000, false),
        )
        .await;
    let claimable: AddressAmounts = api
        .answer(&format!("/v1/escrow/claimable/{HOLDER}?chain={CHAIN}"))
        .await;
    assert_eq!(claimable.priced_at, Some(oldest));
    assert_eq!(claimable.priced(), held.priced());
    suite.store().forget().await;
    suite.done();
}

/// An address's totals value each named token at the served prices, the
/// chain's coin first, and sum its USD per chain and across chains; a token
/// nobody prices is not summed.
#[tokio::test]
async fn totals_are_valued_at_the_served_prices() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    let ether_price = Coin {
        timestamp: now() - 100,
        ..Coin::priced(2000.5, 18, "ETH")
    };
    let oldest = ether_price.timestamp;
    mock.price(NATIVE, ether_price);
    mock.price(USDC, Coin::priced(0.9998, 6, "USDC"));
    mock.price(DAI, Coin::priced(1.0, 18, "DAI"));
    // DAI is priced but held in no slot: its seek finds nothing, not the
    // junk token above it.
    let deposits = vec![
        NamesEvent::deposited(NATIVE, SENDER, ether(15)),
        NamesEvent::deposited(USDC, PAYER, U256::from(1_234_567u64)),
        NamesEvent::forwarded(DAI, PAYER, U256::from(3u64)),
        NamesEvent::deposited(JUNK, SENDER, U256::from(5u64)),
    ];
    suite.apply_tx(10, deposits).await;
    suite
        .apply_tx(
            11,
            NamesEvent::bind(HOLDER, KnownPlatform::X, "901", "dave", 5_000, false),
        )
        .await;

    let totals: AddressTotals = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router()
        .answer(&format!("/v1/escrow/totals/{HOLDER}"))
        .await;
    let held = |amount: U512, value: &str, usd_value: &str| Amount {
        amount,
        value: Some(value.to_string()),
        usd: Some(usd(usd_value)),
    };
    let token =
        |token, (symbol, decimals): (&str, u8), claimable, awaiting| TokenTotals {
            token,
            symbol: Some(symbol.to_string()),
            decimals: Some(decimals),
            claimable,
            awaiting,
        };
    let chain_usd = Totals {
        claimable: usd("3001.98"),
        awaiting: usd("0.00"),
    };
    assert_eq!(
        totals,
        AddressTotals {
            address: HOLDER,
            priced_at: Some(oldest),
            chains: vec![ChainTotals {
                chain_id: CHAIN,
                tokens: vec![
                    token(
                        NATIVE,
                        ("ETH", 18),
                        held(ether(15).to(), "1.5", "3000.75"),
                        held(U512::ZERO, "0", "0.00"),
                    ),
                    token(
                        USDC,
                        ("USDC", 6),
                        held(U512::from(1_234_567u64), "1.234567", "1.23"),
                        held(U512::ZERO, "0", "0.00"),
                    ),
                ],
                usd: chain_usd.clone(),
            }],
            usd: chain_usd,
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// When the `/v1/stats` computation fails, escrow amounts still answer, in
/// raw units and with no `pricedAt`.
#[tokio::test]
async fn a_failed_stats_computation_leaves_escrow_amounts_unpriced() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(NATIVE, Coin::priced(2000.5, 18, "ETH"));
    suite
        .apply(10, NamesEvent::deposited(NATIVE, SENDER, ether(15)))
        .await;
    let mut lock = suite.pool.begin().await.expect("a transaction");
    sqlx::query("LOCK TABLE names.stats_totals IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .expect("the stats rollups locked");

    let held: HandleAmounts = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .stats_timing(Duration::ZERO, Duration::ZERO)
        .router()
        .answer(&format!("/v1/escrow/handle/x/dave?chain={CHAIN}"))
        .await;
    lock.rollback().await.expect("the lock released");
    assert_eq!(held.priced_at, None);
    assert_eq!(
        held.priced(),
        BTreeMap::from([(NATIVE, TokenValue::default())])
    );
    suite.store().forget().await;
    suite.done();
}

/// While the `/v1/stats` computation fails, a burst of escrow requests
/// waits on one computation, not one each: the requests after it answer
/// unpriced at once.
#[tokio::test]
async fn escrow_requests_wait_on_one_failing_computation() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(NATIVE, Coin::priced(2000.5, 18, "ETH"));
    suite
        .apply(10, NamesEvent::deposited(NATIVE, SENDER, ether(15)))
        .await;
    let mut lock = suite.pool.begin().await.expect("a transaction");
    sqlx::query("LOCK TABLE names.stats_totals IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .expect("the stats rollups locked");
    let deadline = Duration::from_secs(1);
    let api = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .stats_timing(STATS_TTL, deadline)
        .router();

    let started = Instant::now();
    let path = format!("/v1/escrow/handle/x/dave?chain={CHAIN}");
    let (first, second): (HandleAmounts, HandleAmounts) =
        tokio::join!(api.answer(&path), api.answer(&path));
    let waited = started.elapsed();
    lock.rollback().await.expect("the lock released");
    assert_eq!((first.priced_at, second.priced_at), (None, None));
    assert!(
        waited >= deadline && waited < 2 * deadline,
        "two requests waited {waited:?}"
    );
    // `/v1/stats` and its days read the same backoff, with a code a client
    // retries on.
    for path in ["/v1/stats", "/v1/stats/days"] {
        assert_eq!(
            api.refusal(path).await,
            (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::StatsBackingOff),
            "{path}"
        );
    }
    suite.store().forget().await;
    suite.done();
}

/// A chain list that fails after a good one keeps the chains it named:
/// the chain's tokens are still asked and valued.
#[tokio::test]
async fn a_failed_chain_list_keeps_the_last_one() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(NATIVE, Coin::priced(2000.0, 18, "ETH"));
    let source = mock.source();
    let api = || {
        AppState::new(Store::new(suite.pool.clone()))
            .pricing(Arc::clone(&source))
            .router()
    };
    suite
        .apply(10, NamesEvent::deposited(NATIVE, SENDER, ether(10)))
        .await;
    let first = api().stats().await;
    assert_eq!(first.chain(CHAIN).total.escrow.deposits.usd, usd("2000.00"));

    mock.fail_chains();
    mock.asked();
    suite
        .apply(11, NamesEvent::deposited(NATIVE, PAYER, ether(10)))
        .await;
    let second = api().stats().await;
    assert_eq!(mock.asked(), vec![coin_id(NATIVE)]);
    assert_eq!(
        second.chain(CHAIN).total.escrow.deposits.usd,
        usd("4000.00")
    );
    suite.store().forget().await;
    suite.done();
}

/// A chain the source's list stops naming is not priced: nothing of it is
/// asked, and its coin and tokens priced before read unpriced.
#[tokio::test]
async fn a_chain_the_list_leaves_out_is_not_priced() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(NATIVE, Coin::priced(2000.0, 18, "ETH"));
    mock.price(USDC, Coin::priced(1.0, 6, "USDC"));
    let source = mock.source();
    let api = || {
        AppState::new(Store::new(suite.pool.clone()))
            .pricing(Arc::clone(&source))
            .router()
    };
    let deposits = vec![
        NamesEvent::deposited(NATIVE, SENDER, ether(10)),
        NamesEvent::deposited(USDC, PAYER, U256::from(1_000_000u64)),
    ];
    suite.apply_tx(10, deposits).await;
    let listed = api().stats().await;
    assert_eq!(
        listed.chain(CHAIN).total.escrow.deposits.usd,
        usd("2001.00")
    );

    mock.unlist();
    mock.asked();
    let unlisted = api().stats().await;
    assert_eq!(mock.asked(), Vec::<String>::new());
    assert_eq!(unlisted.priced_at, None);
    assert_eq!(
        unlisted.chain(CHAIN).total.escrow.deposits,
        TokenFlows {
            tokens: vec![TokenFlow::unpriced(NATIVE, ether(10).to())],
            usd: Usd::default(),
            other: Some(OtherTokens { count: 1 }),
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// A price older than a day values nothing: the token reads unpriced
/// rather than stale.
#[tokio::test]
async fn a_price_older_than_a_day_is_not_used() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(
        NATIVE,
        Coin {
            timestamp: now() - DAY - 60,
            ..Coin::priced(2000.0, 18, "ETH")
        },
    );
    suite
        .apply(10, NamesEvent::deposited(NATIVE, SENDER, ether(10)))
        .await;

    let stats = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router()
        .stats()
        .await;
    assert_eq!(stats.priced_at, None);
    assert_eq!(
        stats.chain(CHAIN).total.escrow.deposits.tokens,
        [TokenFlow::unpriced(NATIVE, ether(10).to())]
    );
    suite.store().forget().await;
    suite.done();
}

/// Each chain asks prices for its coin, the tokens it kept, and at most
/// [`CANDIDATES_PER_CHAIN`] others, the most moved first. A token priced
/// once stays asked: tokens anybody writes and moves more cannot push it
/// out.
#[tokio::test]
async fn a_token_priced_before_keeps_its_place() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(USDC, Coin::priced(1.0, 6, "USDC"));
    let source = mock.source();
    let api = |source: &Arc<Prices>| {
        AppState::new(Store::new(suite.pool.clone()))
            .pricing(Arc::clone(source))
            .router()
    };
    suite
        .apply(10, NamesEvent::deposited(USDC, PAYER, U256::from(1_000u64)))
        .await;
    let first = api(&source).stats().await;
    assert_eq!(first.chain(CHAIN).total.escrow.deposits.usd, usd("0.00"));
    assert_eq!(mock.asked(), [coin_id(USDC)]);

    let junk = (0..=CANDIDATES_PER_CHAIN as u64)
        .map(|i| Address::from_word(U256::from(1_000 + i).into()))
        .flat_map(|token| {
            [
                NamesEvent::deposited(token, SENDER, U256::from(1u64)),
                NamesEvent::deposited(token, SENDER, U256::from(1u64)),
            ]
        })
        .collect();
    suite.apply_tx(11, junk).await;

    let kept = api(&source).stats().await;
    let asked = mock.asked();
    assert!(asked.contains(&coin_id(USDC)), "{asked:?}");
    assert_eq!(asked.len() as i64, CANDIDATES_PER_CHAIN + 1);
    let named: Vec<Address> = kept
        .chain(CHAIN)
        .total
        .escrow
        .deposits
        .tokens
        .iter()
        .map(|flow| flow.token)
        .collect();
    assert_eq!(named, [USDC]);

    let fresh = api(&mock.source()).stats().await;
    let asked = mock.asked();
    assert!(!asked.contains(&coin_id(USDC)), "{asked:?}");
    assert_eq!(asked.len() as i64, CANDIDATES_PER_CHAIN);
    assert!(fresh.chain(CHAIN).total.escrow.deposits.tokens.is_empty());
    suite.store().forget().await;
    suite.done();
}

/// A token's value past the arithmetic, or decimals past it, leaves the
/// token unpriced under `other` and the answer served: anybody chooses an amount, and the
/// source chooses a price and decimals.
#[tokio::test]
async fn a_value_past_the_arithmetic_leaves_its_token_unpriced() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(USDC, Coin::priced(1e60, 0, "HUGE"));
    mock.price(JUNK, Coin::priced(1.0, 255, "WIDE"));
    let deposits = vec![
        NamesEvent::deposited(USDC, PAYER, U256::MAX),
        NamesEvent::deposited(JUNK, SENDER, U256::from(1u64)),
    ];
    suite.apply_tx(10, deposits).await;

    let stats = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router()
        .stats()
        .await;
    assert_eq!(
        stats.chain(CHAIN).total.escrow.deposits,
        TokenFlows {
            tokens: Vec::new(),
            usd: Usd::default(),
            other: Some(OtherTokens { count: 2 }),
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// A symbol is its creator's text: one past 32 characters is not served,
/// and the token is still valued.
#[tokio::test]
async fn a_long_symbol_is_not_served() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(USDC, Coin::priced(1.0, 6, &"U".repeat(33)));
    suite
        .apply(
            10,
            NamesEvent::deposited(USDC, PAYER, U256::from(2_500_000u64)),
        )
        .await;

    let stats = AppState::new(Store::new(suite.pool.clone()))
        .pricing(mock.source())
        .router()
        .stats()
        .await;
    let flows = stats.chain(CHAIN).total.escrow.deposits;
    assert_eq!(flows.tokens[0].priced.symbol, None);
    assert_eq!(flows.tokens[0].priced.usd, Some(usd("2.50")));
    suite.store().forget().await;
    suite.done();
}

/// Junk tokens moved more each round, n per round.
fn junk(from: u64, count: u64) -> Vec<NamesEvent> {
    (from..from + count)
        .map(|i| Address::from_word(U256::from(i).into()))
        .flat_map(|token| {
            [
                NamesEvent::deposited(token, SENDER, U256::from(1u64)),
                NamesEvent::deposited(token, SENDER, U256::from(1u64)),
            ]
        })
        .collect()
}

/// Dust of tokens the source prices joins the kept tokens 40 per
/// computation, but each chain keeps only the [`KEPT_PER_CHAIN`] that moved
/// the most USD: a token with real volume stays named, and the coins asked
/// stop growing at the kept and the new.
#[tokio::test]
async fn priced_dust_cannot_grow_the_tokens_asked() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(USDC, Coin::priced(1.0, 6, "USDC"));
    let dust = 2_000..2_000 + 3 * CANDIDATES_PER_CHAIN as u64 + 30;
    for i in dust.clone() {
        let token = Address::from_word(U256::from(i).into());
        mock.price(token, Coin::priced(1.0, 6, "DUST"));
    }
    let source = mock.source();
    let api = || {
        AppState::new(Store::new(suite.pool.clone()))
            .pricing(Arc::clone(&source))
            .router()
    };
    let paid = NamesEvent::deposited(USDC, PAYER, U256::from(1_000_000_000u64));
    suite.apply(10, paid).await;
    api().stats().await;
    mock.asked();
    suite
        .apply_tx(11, junk(dust.start, dust.end - dust.start))
        .await;

    let mut asked = Vec::new();
    let mut named = Vec::new();
    for _ in 0..5 {
        let stats = api().stats().await;
        asked.push(mock.asked().len());
        named = stats
            .chain(CHAIN)
            .total
            .escrow
            .deposits
            .tokens
            .iter()
            .map(|flow| flow.token)
            .collect();
    }
    let new = CANDIDATES_PER_CHAIN as usize;
    assert_eq!(
        asked,
        [
            1 + new,
            1 + 2 * new,
            1 + 3 * new,
            KEPT_PER_CHAIN + new,
            KEPT_PER_CHAIN + new
        ]
    );
    assert_eq!(named.len(), KEPT_PER_CHAIN);
    assert!(named.contains(&USDC), "{named:?}");
    suite.store().forget().await;
    suite.done();
}

/// A kept token answered once without confidence is not valued that
/// round, stays asked, and is valued again once the source is confident.
#[tokio::test]
async fn a_confidence_dip_keeps_a_token_asked() {
    let suite = Suite::open(&SUITE, CHAIN).await;
    let mock = MockSource::start().await;
    mock.price(USDC, Coin::priced(1.0, 6, "USDC"));
    let source = mock.source();
    let api = || {
        AppState::new(Store::new(suite.pool.clone()))
            .pricing(Arc::clone(&source))
            .router()
    };
    let usdc_named = |stats: Stats| {
        stats
            .chain(CHAIN)
            .total
            .escrow
            .deposits
            .tokens
            .iter()
            .any(|flow| flow.token == USDC)
    };
    suite
        .apply(10, NamesEvent::deposited(USDC, PAYER, U256::from(1_000u64)))
        .await;
    assert!(usdc_named(api().stats().await));
    suite
        .apply_tx(11, junk(3_000, CANDIDATES_PER_CHAIN as u64 + 1))
        .await;
    mock.asked();

    mock.price(
        USDC,
        Coin {
            confidence: 0.5,
            ..Coin::priced(1.0, 6, "USDC")
        },
    );
    assert!(!usdc_named(api().stats().await));
    assert!(mock.asked().contains(&coin_id(USDC)));

    mock.price(USDC, Coin::priced(1.0, 6, "USDC"));
    assert!(usdc_named(api().stats().await));
    assert!(mock.asked().contains(&coin_id(USDC)));
    suite.store().forget().await;
    suite.done();
}
