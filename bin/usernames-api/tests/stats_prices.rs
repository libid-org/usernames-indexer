//! `GET /v1/stats` valued in USD, against a price source on this machine
//! that answers DefiLlama's `prices/current/<coins>`: a token priced with
//! confidence is named and valued, every other one counts under `other`,
//! a failed request keeps the last prices, and a token priced once keeps
//! its place among the coins asked.
//!
//! Skips without `DATABASE_URL`, unless `CI` is set: there its absence
//! fails. The suite's chain is its own, named to the source as
//! `testchain`; each test forgets it when it ends.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        Mutex as StdMutex,
    },
    time::{
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
            OtherTokens,
            TokenFlow,
            TokenFlows,
            Usd,
        },
        AppState,
        PriceSource,
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
    StatsRoute,
    Suite,
};

const CHAIN: i64 = 31353;
/// The suite chain's name to the source.
const CHAIN_NAME: &str = "testchain";

static SUITE: Mutex<()> = Mutex::const_new(());

/// The chain's own coin, as the escrow names it (EIP-7528).
const NATIVE: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
const USDC: Address = Address::repeat_byte(0x05);
const JUNK: Address = Address::repeat_byte(0x0B);

const SENDER: Address = Address::repeat_byte(0x51);
const PAYER: Address = Address::repeat_byte(0x52);
const HOLDER: Address = Address::repeat_byte(0xA1);

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

/// What the source prices and whether it answers, and the coin ids it was
/// asked for.
#[derive(Default)]
struct Book {
    coins: BTreeMap<String, Coin>,
    failing: bool,
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

/// A price source serving on a port of its own until dropped.
struct MockSource {
    base: Url,
    book: Arc<StdMutex<Book>>,
    server: JoinHandle<()>,
}

impl MockSource {
    async fn start() -> Self {
        let book = Arc::<StdMutex<Book>>::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("its address");
        let base = format!("http://{addr}").parse().expect("a URL");
        let app = Router::new()
            .route("/prices/current/{coins}", get(answer))
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

    /// The coin ids asked since the last call.
    fn asked(&self) -> Vec<String> {
        let mut book = self.book.lock().expect("the book");
        let asked = std::mem::take(&mut book.asked);
        drop(book);
        asked
    }

    /// A price source over this one, naming the suite's chain.
    fn source(&self) -> Arc<PriceSource> {
        Arc::new(PriceSource::new(self.base.clone()).naming(CHAIN, CHAIN_NAME))
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
            symbol: None,
            decimals: None,
            value: None,
            usd: None,
        }
    }

    fn valued(
        self,
        (symbol, decimals): (&str, u8),
        value: &str,
        usd_value: &str,
    ) -> Self {
        Self {
            symbol: Some(symbol.to_string()),
            decimals: Some(decimals),
            value: Some(value.to_string()),
            usd: Some(usd(usd_value)),
            ..self
        }
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
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
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
            other: Some(OtherTokens {
                count: 1,
                wallets: 1,
            }),
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

/// A token the source does not know, and one it prices with confidence
/// under 0.9, count under `other`; both were asked.
#[tokio::test]
async fn a_token_without_a_confident_price_counts_under_other() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
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
            other: Some(OtherTokens {
                count: 2,
                wallets: 2,
            }),
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
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
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
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
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
            other: Some(OtherTokens {
                count: 1,
                wallets: 1,
            }),
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// A price older than a day values nothing: the token reads unpriced
/// rather than stale.
#[tokio::test]
async fn a_price_older_than_a_day_is_not_used() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
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

/// Each chain asks prices for at most [`CANDIDATES_PER_CHAIN`] tokens
/// beyond its coin, the most moved first. A token priced once stays asked:
/// tokens anybody writes and moves more cannot push it out.
#[tokio::test]
async fn a_token_priced_before_keeps_its_place() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let mock = MockSource::start().await;
    mock.price(USDC, Coin::priced(1.0, 6, "USDC"));
    let source = mock.source();
    let api = |source: &Arc<PriceSource>| {
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
/// token unpriced and the answer served: anybody chooses an amount, and the
/// source chooses a price and decimals.
#[tokio::test]
async fn a_value_past_the_arithmetic_leaves_its_token_unpriced() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
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
            tokens: vec![TokenFlow::unpriced(USDC, U256::MAX.to())],
            usd: Usd::default(),
            other: Some(OtherTokens {
                count: 1,
                wallets: 1,
            }),
        }
    );
    suite.store().forget().await;
    suite.done();
}

/// A symbol is its creator's text: one past 32 characters is not served,
/// and the token is still valued.
#[tokio::test]
async fn a_long_symbol_is_not_served() {
    let Some(suite) = Suite::open(&SUITE, CHAIN).await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
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
    assert_eq!(flows.tokens[0].symbol, None);
    assert_eq!(flows.tokens[0].usd, Some(usd("2.50")));
    suite.store().forget().await;
    suite.done();
}
