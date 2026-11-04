//! USD prices for `/v1/stats`, from a source that answers DefiLlama's
//! `prices/current/<coins>`: each coin's price, decimals, symbol, confidence
//! and the time it was priced. The store keeps token units; these prices
//! value them when the answer is computed.

use std::{
    collections::HashMap,
    future::Future,
    ops::RangeInclusive,
    str::FromStr,
    sync::{
        Mutex,
        MutexGuard,
    },
    time::{
        Duration,
        SystemTime,
        UNIX_EPOCH,
    },
};

use alloy::primitives::{
    Address,
    U512,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::task::JoinSet;
use url::Url;

use super::model::Usd;
use crate::{
    chain::NATIVE_COIN,
    db::{
        Candidate,
        ChainToken,
        NamedTokens,
    },
};

/// The source DefiLlama runs, with no key.
pub const DEFILLAMA: &str = "https://coins.llama.fi";

/// The chains the source names, by chain id; a chain missing here, a
/// testnet, is never priced.
const CHAINS: [(i64, &str); 5] = [
    (1, "ethereum"),
    (42161, "arbitrum"),
    (8453, "base"),
    (56, "bsc"),
    (714, "eden"),
];

/// How the source names a chain's own coin.
const SOURCE_COIN: Address = Address::ZERO;

/// The least confidence a price is used at. The source scores a price by
/// the liquidity of the markets it reads; real tokens score 0.99 and above.
const CONFIDENCE: f64 = 0.9;

/// Coins per request, about 5 KB of path.
const BATCH: usize = 100;

/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The largest answer read, against a source that keeps sending.
const MAX_BODY: usize = 1 << 20;

/// How old a held price may be and still value a token.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The most decimals a token may have and be priced: `10^(decimals + 16)`
/// must fit the arithmetic.
const MAX_DECIMALS: u32 = 77;

/// The priced tokens a chain names and keeps asking for, those that moved
/// the most USD at their prices: a deposit of dust cannot buy a place. With
/// [`CANDIDATES_PER_CHAIN`](crate::db::CANDIDATES_PER_CHAIN) new ones, a computation asks at most 141 coins
/// per chain.
pub const KEPT_PER_CHAIN: usize = 100;

/// The longest symbol served; a token's creator writes it.
const MAX_SYMBOL: usize = 32;

/// Format characters that reorder text or take no width, which a symbol
/// could use to pass for another: zero-width spaces and joiners, the bidi
/// marks, embeddings, overrides and isolates.
const HIDDEN: [RangeInclusive<char>; 3] = [
    '\u{200B}'..='\u{200F}',
    '\u{202A}'..='\u{202E}',
    '\u{2066}'..='\u{2069}',
];

/// A price in units of 10^-18 USD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Price(U512);

/// Why a price does not parse.
#[derive(Debug, thiserror::Error)]
#[error("not a price: {0}")]
pub(crate) struct PriceError(String);

impl FromStr for Price {
    type Err = PriceError;

    /// A non-negative JSON number, `3412.53`, `1` or `1.2e-5`, exactly;
    /// digits past 10^-18 are dropped.
    fn from_str(text: &str) -> Result<Self, PriceError> {
        let refused = || PriceError(text.to_string());
        let (mantissa, exponent) = match text.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => {
                (mantissa, exponent.parse::<i32>().map_err(|_| refused())?)
            }
            None => (text, 0),
        };
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits = format!("{whole}{fraction}");
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(refused());
        }
        let fraction_len = i32::try_from(fraction.len()).map_err(|_| refused())?;
        let shift = exponent
            .checked_add(18)
            .and_then(|shift| shift.checked_sub(fraction_len))
            .ok_or_else(refused)?;
        let digits = U512::from_str_radix(&digits, 10).map_err(|_| refused())?;
        let scaled = if shift >= 0 {
            digits.checked_mul(ten_to(shift.unsigned_abs()).ok_or_else(refused)?)
        } else {
            ten_to(shift.unsigned_abs()).map(|divisor| digits / divisor)
        };
        scaled.map(Self).ok_or_else(refused)
    }
}

/// 10^`power`, when it fits.
fn ten_to(power: u32) -> Option<U512> {
    U512::from(10u8).checked_pow(U512::from(power))
}

/// A coin as the source knows it: what it is called, its decimals and its
/// price, with the unix second it was priced at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quote {
    pub(crate) symbol: Option<String>,
    pub(crate) decimals: u8,
    pub(crate) price: Price,
    pub(crate) at: i64,
}

impl Quote {
    /// `amount` token units after the decimals, as a decimal string:
    /// `1500000` at 6 decimals is `1.5`.
    pub(crate) fn value(&self, amount: U512) -> String {
        let digits = amount.to_string();
        let decimals = usize::from(self.decimals);
        if decimals == 0 {
            return digits;
        }
        let padded = format!("{digits:0>width$}", width = decimals + 1);
        let (whole, fraction) = padded.split_at(padded.len() - decimals);
        match fraction.trim_end_matches('0') {
            "" => whole.to_string(),
            fraction => format!("{whole}.{fraction}"),
        }
    }

    /// `amount` token units in USD, truncated to the cent; `None` when the
    /// arithmetic overflows.
    pub(crate) fn usd(&self, amount: U512) -> Option<Usd> {
        let divisor = ten_to(u32::from(self.decimals) + 16)?;
        amount
            .checked_mul(self.price.0)
            .map(|product| Usd::from_cents(product / divisor))
    }

    /// `amount` token units in units of 10^-18 USD, a rank finer than
    /// cents; `None` when the arithmetic overflows.
    fn worth(&self, amount: U512) -> Option<U512> {
        let divisor = ten_to(u32::from(self.decimals))?;
        amount
            .checked_mul(self.price.0)
            .map(|product| product / divisor)
    }
}

/// One coin of a source's answer.
#[derive(Deserialize)]
struct Priced {
    price: Box<RawValue>,
    decimals: Option<u64>,
    symbol: Option<String>,
    confidence: Option<f64>,
    timestamp: i64,
}

/// A source's answer: the coins it knows, by the id asked.
#[derive(Deserialize)]
struct Answer {
    coins: HashMap<String, Priced>,
}

impl Priced {
    /// The symbol as served: at most [`MAX_SYMBOL`] characters, none of
    /// them a control character or one that reorders or hides text.
    fn symbol(&self) -> Option<String> {
        self.symbol
            .as_deref()
            .filter(|symbol| {
                symbol.chars().count() <= MAX_SYMBOL
                    && !symbol.chars().any(|c| {
                        c.is_control() || HIDDEN.iter().any(|hidden| hidden.contains(&c))
                    })
            })
            .map(str::to_string)
    }

    /// The quote this answer gives, when it is confident and every field
    /// holds.
    fn quote(&self) -> Option<Quote> {
        let decimals = self.decimals.filter(|d| *d <= u64::from(MAX_DECIMALS))?;
        if self.confidence? < CONFIDENCE {
            return None;
        }
        Some(Quote {
            symbol: self.symbol(),
            decimals: u8::try_from(decimals).ok()?,
            price: self.price.get().parse().ok()?,
            at: self.timestamp,
        })
    }
}

/// The prices one computation values its tokens at, by coin.
#[derive(Debug, Default)]
pub(crate) struct Quotes(pub(crate) HashMap<ChainToken, Quote>);

impl NamedTokens for Quotes {
    fn named(&self) -> Vec<ChainToken> {
        self.0.keys().copied().collect()
    }
}

impl Quotes {
    /// The unix second of the oldest price held.
    pub(crate) fn taken_at(&self) -> Option<i64> {
        self.0.values().map(|quote| quote.at).min()
    }
}

/// A coin priced with confidence at least once: its last confident quote,
/// and whether the source's latest answer for it was confident too.
#[derive(Clone, Debug)]
struct Known {
    quote: Quote,
    current: bool,
}

/// The coins a source priced with confidence, by coin: the tokens each
/// computation asks again, and the quotes it may value them at.
#[derive(Debug, Default)]
struct Book(HashMap<ChainToken, Known>);

impl Book {
    /// What the source answered for `coin`: a confident quote replaces the
    /// last one, and any other answer leaves it unable to value the coin
    /// while keeping the coin known.
    fn learn(&mut self, coin: ChainToken, priced: &Priced) {
        match priced.quote() {
            Some(quote) => {
                self.0.insert(
                    coin,
                    Known {
                        quote,
                        current: true,
                    },
                );
            }
            None => {
                if let Some(known) = self.0.get_mut(&coin) {
                    known.current = false;
                }
            }
        }
    }

    /// Keep, per chain, the [`KEPT_PER_CHAIN`] known `candidates` that moved
    /// the most USD at their last confident quote, ties by token; forget the
    /// rest.
    fn keep(&mut self, candidates: &[Candidate]) {
        let mut chains: HashMap<i64, Vec<(U512, ChainToken)>> = HashMap::new();
        for candidate in candidates {
            let worth = self
                .0
                .get(&candidate.coin)
                .zip(candidate.moved())
                .and_then(|(known, moved)| known.quote.worth(moved));
            if let Some(worth) = worth {
                let chain = chains.entry(candidate.coin.chain_id).or_default();
                chain.push((worth, candidate.coin));
            }
        }
        let mut kept = HashMap::new();
        for mut ranked in chains.into_values() {
            ranked.sort_by(|(a, x), (b, y)| b.cmp(a).then(x.token.cmp(&y.token)));
            for (_, coin) in ranked.into_iter().take(KEPT_PER_CHAIN) {
                if let Some(known) = self.0.remove(&coin) {
                    kept.insert(coin, known);
                }
            }
        }
        self.0 = kept;
    }

    /// The quotes that value their coins now: the source's latest answer
    /// was confident, and the price is under a day old.
    fn quotes(&self) -> Quotes {
        let oldest = unix_now().saturating_sub(MAX_AGE.as_secs() as i64);
        Quotes(
            self.0
                .iter()
                .filter(|(_, known)| known.current && known.quote.at >= oldest)
                .map(|(coin, known)| (*coin, known.quote.clone()))
                .collect(),
        )
    }
}

/// A price source and the coins it priced with confidence. The one stats
/// computation running at a time asks it, so it has one writer.
pub struct PriceSource {
    base: Url,
    chains: HashMap<i64, String>,
    client: reqwest::Client,
    book: Mutex<Book>,
}
impl PriceSource {
    /// The source at `base`, DefiLlama's [`DEFILLAMA`] or one that answers
    /// the same way.
    pub fn new(base: Url) -> Self {
        Self {
            base,
            chains: CHAINS
                .iter()
                .map(|(chain_id, name)| (*chain_id, name.to_string()))
                .collect(),
            client: reqwest::Client::new(),
            book: Mutex::default(),
        }
    }

    /// The same source, naming `chain_id` `name` too: a test chain priced
    /// by a test's source.
    #[cfg(feature = "test-helpers")]
    pub fn naming(mut self, chain_id: i64, name: &str) -> Self {
        self.chains.insert(chain_id, name.to_string());
        self
    }

    /// The source's id of `coin`: `<chain>:0x…`, the chain's own coin as
    /// the zero address; `None` on a chain the source does not name.
    fn coin_id(&self, coin: &ChainToken) -> Option<String> {
        let chain = self.chains.get(&coin.chain_id)?;
        let token = if coin.token == NATIVE_COIN {
            SOURCE_COIN
        } else {
            coin.token
        };
        Some(format!("{chain}:{token:#x}"))
    }

    /// The coins priced with confidence before and kept, which stay among
    /// those asked whatever their latest answer: a token anybody wrote
    /// cannot push them out.
    pub(crate) fn priced_before(&self) -> Vec<ChainToken> {
        let book = self.book();
        let coins = book.0.keys().copied().collect();
        drop(book);
        coins
    }

    /// Prices for `candidates`, asked now. A coin the source prices with
    /// confidence takes the new quote, and one it answers otherwise is not
    /// valued until it is again; a request that fails keeps the quotes of its
    /// coins. A quote older than a day values nothing. Each chain keeps the
    /// [`KEPT_PER_CHAIN`] priced coins that moved the most USD; the rest are
    /// forgotten and not valued.
    pub(crate) async fn quotes(&self, candidates: &[Candidate]) -> Quotes {
        let ids: HashMap<String, ChainToken> = candidates
            .iter()
            .filter_map(|candidate| {
                Some((self.coin_id(&candidate.coin)?, candidate.coin))
            })
            .collect();
        let mut asked: Vec<&String> = ids.keys().collect();
        asked.sort();
        let mut requests = JoinSet::new();
        for batch in asked.chunks(BATCH) {
            let url = self.url(batch);
            let client = self.client.clone();
            requests.spawn(async move { client.ask_prices(url?).await });
        }
        let mut fresh = HashMap::new();
        while let Some(answer) = requests.join_next().await {
            match answer {
                Ok(Ok(answer)) => fresh.extend(answer.coins),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "a price request failed; its coins keep their last prices")
                }
                Err(stopped) => {
                    tracing::warn!(%stopped, "a price request stopped; its coins keep their last prices")
                }
            }
        }
        let mut book = self.book();
        for (id, priced) in &fresh {
            if let Some(coin) = ids.get(id) {
                book.learn(*coin, priced);
            }
        }
        book.keep(candidates);
        let quotes = book.quotes();
        drop(book);
        quotes
    }

    /// The request for one batch of coin ids.
    fn url(&self, batch: &[&String]) -> Result<Url, PriceFetchError> {
        let ids: Vec<&str> = batch.iter().map(|id| id.as_str()).collect();
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| PriceFetchError::Base(self.base.clone()))?
            .pop_if_empty()
            .extend(["prices", "current", &ids.join(",")]);
        Ok(url)
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A client asking a price source.
trait AskPrices {
    /// One request, its answer read up to [`MAX_BODY`].
    fn ask_prices(
        &self,
        url: Url,
    ) -> impl Future<Output = Result<Answer, PriceFetchError>> + Send;
}

impl AskPrices for reqwest::Client {
    async fn ask_prices(&self, url: Url) -> Result<Answer, PriceFetchError> {
        let mut response = self
            .get(url)
            .timeout(TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(PriceFetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

/// Why a price request gave nothing.
#[derive(Debug, thiserror::Error)]
enum PriceFetchError {
    #[error("{0} cannot hold a path")]
    Base(Url),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("the answer is past {MAX_BODY} bytes")]
    TooLarge,
    #[error("the answer does not parse: {0}")]
    Shape(#[from] serde_json::Error),
}

/// Unix seconds now.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(text: &str) -> U512 {
        text.parse::<Price>().expect("a price").0
    }

    #[test]
    fn a_price_parses_exactly_in_either_notation() {
        let unit = U512::from(10u8).pow(U512::from(18u8));
        assert_eq!(price("1"), unit);
        assert_eq!(
            price("3412.53"),
            U512::from(341_253u64) * unit / U512::from(100u8)
        );
        assert_eq!(price("1.2e-5"), U512::from(12_000_000_000_000u64));
        assert_eq!(price("4.732E-06"), U512::from(4_732_000_000_000u64));
        for refused in ["", "-1", "1.2.3", "abc", "1e", "1e-99999999999"] {
            assert!(refused.parse::<Price>().is_err(), "{refused}");
        }
    }

    #[test]
    fn a_coin_id_names_the_chain_and_the_source_coin() {
        let source = PriceSource::new(DEFILLAMA.parse().expect("a URL"));
        let usdc = ChainToken {
            chain_id: 8453,
            token: Address::repeat_byte(0xAB),
        };
        assert_eq!(
            source.coin_id(&usdc).as_deref(),
            Some("base:0xabababababababababababababababababababab")
        );
        let coin = ChainToken {
            chain_id: 1,
            token: NATIVE_COIN,
        };
        assert_eq!(
            source.coin_id(&coin).as_deref(),
            Some("ethereum:0x0000000000000000000000000000000000000000")
        );
        let testnet = ChainToken {
            chain_id: 84532,
            token: NATIVE_COIN,
        };
        assert_eq!(source.coin_id(&testnet), None);
    }

    #[test]
    fn a_symbol_is_served_only_short_and_without_control_characters() {
        let symbol = |symbol: &str| {
            Priced {
                price: RawValue::from_string("1".to_string()).expect("a number"),
                decimals: Some(6),
                symbol: Some(symbol.to_string()),
                confidence: Some(0.99),
                timestamp: 0,
            }
            .symbol()
        };
        assert_eq!(symbol("USDC").as_deref(), Some("USDC"));
        assert_eq!(symbol(&"A".repeat(33)), None);
        assert_eq!(symbol("US\u{7}DC"), None);
        assert_eq!(symbol("US\nDC"), None);
        assert_eq!(symbol("US\u{202E}DC"), None);
        assert_eq!(symbol("US\u{200B}DC"), None);
        assert_eq!(symbol("US\u{2066}DC"), None);
    }
}
