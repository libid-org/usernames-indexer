//! USD prices for `/v1/stats`, from a [`PriceSource`]: each coin's price,
//! decimals, symbol, confidence and the time it was priced, and the chains
//! the source names. The store keeps token units; these prices value them
//! when the answer is computed.

use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    fmt,
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
use serde::{
    de::{
        DeserializeOwned,
        IgnoredAny,
    },
    Deserialize,
};
use serde_json::value::RawValue;
use tokio::task::JoinSet;
use url::Url;

use super::model::{
    TokenValue,
    Usd,
};
use crate::{
    chain::NATIVE_COIN,
    db::{
        Candidate,
        ListedAddress,
        NamedTokens,
    },
};

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

/// The priced tokens a chain names and keeps asking for beside its coin:
/// those valued now first, then those that moved the most USD at their last
/// confident prices. A deposit of dust cannot buy a place, and a token that
/// lost its price cannot hold one against a token that has one. With
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

/// Where `/v1/stats` reads USD prices and the chains they are for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PriceSource {
    /// DefiLlama, with no key: prices from `coins.llama.fi`, chains from
    /// `api.llama.fi/v2/chains`, each named by its entry's `name`,
    /// lowercased.
    #[default]
    DefiLlama,
}

/// Why a price source does not parse.
#[derive(Debug, thiserror::Error)]
#[error("not a price source; the one known is `defillama`: {0:?}")]
pub struct PriceSourceError(String);

impl FromStr for PriceSource {
    type Err = PriceSourceError;

    fn from_str(text: &str) -> Result<Self, PriceSourceError> {
        match text {
            "defillama" => Ok(Self::DefiLlama),
            _ => Err(PriceSourceError(text.to_string())),
        }
    }
}

impl fmt::Display for PriceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefiLlama => f.write_str("defillama"),
        }
    }
}

impl PriceSource {
    /// Where a server on `base` answering as this source does answers.
    #[cfg(feature = "test-helpers")]
    fn endpoints_under(self, base: &Url) -> Endpoints {
        let base = base.as_str().trim_end_matches('/');
        match self {
            Self::DefiLlama => Endpoints {
                prices: base.to_string(),
                chains: format!("{base}/v2/chains"),
            },
        }
    }

    /// Where the source answers.
    fn endpoints(self) -> Endpoints {
        match self {
            Self::DefiLlama => Endpoints {
                prices: "https://coins.llama.fi".to_string(),
                chains: "https://api.llama.fi/v2/chains".to_string(),
            },
        }
    }

    /// The chains the source lists at `endpoint`, named as it names them
    /// in coin ids.
    async fn chain_names(
        self,
        client: &reqwest::Client,
        endpoint: &str,
    ) -> Result<ChainNames, PriceFetchError> {
        let url = Url::parse(endpoint)?;
        match self {
            Self::DefiLlama => Ok(ListedChain::names(client.fetch(url).await?)),
        }
    }

    /// The source's id of `coin` among `chains`: `<chain>:0x…`, the
    /// chain's own coin as the zero address; `None` on a chain the source
    /// does not name.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    fn coin_id(self, chains: &ChainNames, coin: &ListedAddress) -> Option<String> {
        match self {
            Self::DefiLlama => {
                let chain = chains.name(coin.chain_id)?;
                let token = if coin.address == NATIVE_COIN {
                    SOURCE_COIN
                } else {
                    coin.address
                };
                Some(format!("{chain}:{token:#x}"))
            }
        }
    }

    /// The request under `endpoint` for one batch of coin ids.
    fn price_url(self, endpoint: &str, ids: &[&String]) -> Result<Url, PriceFetchError> {
        let ids: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
        let mut url = Url::parse(endpoint)?;
        match self {
            Self::DefiLlama => {
                url.path_segments_mut()
                    .map_err(|()| PriceFetchError::Base(endpoint.to_string()))?
                    .pop_if_empty()
                    .extend(["prices", "current", &ids.join(",")]);
            }
        }
        Ok(url)
    }

    /// What the source answers at `url`, by coin id: each coin's quote,
    /// or `None` where the answer is not confident or does not hold.
    async fn prices(
        self,
        client: reqwest::Client,
        url: Url,
    ) -> Result<HashMap<String, Option<Quote>>, PriceFetchError> {
        match self {
            Self::DefiLlama => {
                let answer: Answer = client.fetch(url).await?;
                Ok(answer
                    .coins
                    .into_iter()
                    .map(|(id, priced)| (id, priced.quote()))
                    .collect())
            }
        }
    }
}

/// Where a source answers: its prices under `prices`, its chain list at
/// `chains`.
#[derive(Clone, Debug)]
struct Endpoints {
    prices: String,
    chains: String,
}

/// A chain id as a source lists it: a number, a decimal string, or none.
#[derive(Deserialize)]
#[serde(untagged)]
enum ListedChainId {
    Number(i64),
    Text(String),
    Other(IgnoredAny),
}

impl ListedChainId {
    fn chain_id(&self) -> Option<i64> {
        match self {
            Self::Number(chain_id) => Some(*chain_id),
            Self::Text(text) => text.parse().ok(),
            Self::Other(_) => None,
        }
    }
}

/// One entry of DefiLlama's chain list.
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Deserialize)]
struct ListedChain {
    #[serde(rename = "chainId")]
    chain_id: Option<ListedChainId>,
    name: String,
}

impl ListedChain {
    /// Each listed chain with an id, named by its entry's `name`,
    /// lowercased.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    fn names(listed: Vec<Self>) -> ChainNames {
        ChainNames::of(listed.into_iter().filter_map(|entry| {
            let chain_id = entry.chain_id.as_ref().and_then(ListedChainId::chain_id)?;
            Some((chain_id, entry.name.to_lowercase()))
        }))
    }
}

/// The name the source gives each chain it prices, by chain id. A chain
/// missing here, a testnet, is never priced.
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ChainNames(pub BTreeMap<i64, String>);

impl ChainNames {
    /// The `named` chains whose names fit a price request's path: ASCII
    /// letters, digits, spaces, `-`, `_` and `.`. Where names share an id
    /// (56 is `bsc` and `binance`), the least is kept, so a reordered list
    /// names the chain the same way.
    fn of(named: impl IntoIterator<Item = (i64, String)>) -> Self {
        let mut chains: BTreeMap<i64, String> = BTreeMap::new();
        for (chain_id, name) in named {
            let usable = !name.is_empty()
                && name.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-' | b'_' | b'.')
                });
            if !usable {
                continue;
            }
            chains
                .entry(chain_id)
                .and_modify(|held| {
                    if name < *held {
                        held.clone_from(&name);
                    }
                })
                .or_insert(name);
        }
        Self(chains)
    }

    /// The source's name for `chain_id`.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    fn name(&self, chain_id: i64) -> Option<&str> {
        self.0.get(&chain_id).map(String::as_str)
    }
}

/// A price in units of 10^-18 USD.
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Price(pub U512);

/// Why a price does not parse.
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
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
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quote {
    /// What the coin is called, when the source names it.
    pub symbol: Option<String>,
    /// Its decimals.
    pub decimals: u8,
    /// Its price.
    pub price: Price,
    /// The unix second it was priced at.
    pub at: i64,
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

    /// `amount` token units valued at this quote; unpriced when the
    /// arithmetic overflows.
    fn token_value(&self, amount: U512) -> TokenValue {
        match self.usd(amount) {
            Some(usd) => TokenValue {
                symbol: self.symbol.clone(),
                decimals: Some(self.decimals),
                value: Some(self.value(amount)),
                usd: Some(usd),
            },
            None => TokenValue::default(),
        }
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
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Deserialize)]
struct Priced {
    /// The price, as the source writes the number.
    pub price: Box<RawValue>,
    /// The coin's decimals.
    pub decimals: Option<u64>,
    /// What the coin is called.
    pub symbol: Option<String>,
    /// How sure the source is of the price.
    pub confidence: Option<f64>,
    /// The unix second it was priced at.
    pub timestamp: i64,
}

/// A source's answer: the coins it knows, by the id asked.
#[derive(Deserialize)]
struct Answer {
    coins: HashMap<String, Priced>,
}

impl Priced {
    /// The symbol as served: at most [`MAX_SYMBOL`] characters, none of
    /// them a control character or one that reorders or hides text.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
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
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Debug, Default)]
pub(crate) struct Quotes(pub HashMap<ListedAddress, Quote>);

impl NamedTokens for Quotes {
    /// The priced tokens; each chain's coin is named whether priced or not.
    fn named(&self) -> Vec<ListedAddress> {
        self.0
            .keys()
            .filter(|coin| coin.address != NATIVE_COIN)
            .copied()
            .collect()
    }
}

impl Quotes {
    /// The quote of `token` on `chain_id`, when it is priced.
    pub(crate) fn of(&self, chain_id: i64, token: Address) -> Option<&Quote> {
        self.0.get(&ListedAddress {
            chain_id,
            address: token,
        })
    }

    /// The unix second of the oldest price held.
    pub(crate) fn taken_at(&self) -> Option<i64> {
        self.0.values().map(|quote| quote.at).min()
    }
}

/// A coin priced with confidence at least once: its last confident quote,
/// and whether the source's latest answer for it was confident too.
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Clone, Debug)]
struct Known {
    /// Its last confident quote.
    pub quote: Quote,
    /// Whether the source's latest answer for it was confident.
    pub current: bool,
}

impl Known {
    /// Whether the quote values its coin: the source's latest answer was
    /// confident, and the price is from `oldest` on.
    fn values(&self, oldest: i64) -> bool {
        self.current && self.quote.at >= oldest
    }
}

/// The coins a source priced with confidence, by coin: the tokens each
/// computation asks again, and the quotes it may value them at.
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
#[derive(Debug, Default)]
struct Book(pub HashMap<ListedAddress, Known>);

impl Book {
    /// What the source answered for `coin`: a confident quote replaces the
    /// last one, and any other answer leaves it unable to value the coin
    /// while keeping the coin known.
    fn learn(&mut self, coin: ListedAddress, quote: Option<Quote>) {
        match quote {
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

    /// Keep each chain's own coin and, per chain, the [`KEPT_PER_CHAIN`]
    /// known `candidates` ranked by whether they value now, then by the USD
    /// they moved at their last confident quote, ties by token; forget the
    /// rest. A coin that lost its price never outranks one that has one.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    fn keep<'a>(&mut self, candidates: impl IntoIterator<Item = &'a Candidate>) {
        let oldest = Self::oldest();
        let mut chains: HashMap<i64, Vec<(bool, U512, ListedAddress)>> = HashMap::new();
        let mut kept = HashMap::new();
        for candidate in candidates {
            let Some(known) = self.0.get(&candidate.coin) else {
                continue;
            };
            if candidate.coin.address == NATIVE_COIN {
                kept.insert(candidate.coin, known.clone());
                continue;
            }
            let worth = candidate.moved().and_then(|moved| known.quote.worth(moved));
            if let Some(worth) = worth {
                let chain = chains.entry(candidate.coin.chain_id).or_default();
                chain.push((known.values(oldest), worth, candidate.coin));
            }
        }
        for mut ranked in chains.into_values() {
            ranked.sort_by(|(now_a, a, x), (now_b, b, y)| {
                now_b
                    .cmp(now_a)
                    .then(b.cmp(a))
                    .then(x.address.cmp(&y.address))
            });
            for (_, _, coin) in ranked.into_iter().take(KEPT_PER_CHAIN) {
                if let Some(known) = self.0.get(&coin) {
                    kept.insert(coin, known.clone());
                }
            }
        }
        self.0 = kept;
    }

    /// The oldest unix second a quote may be from and still value its coin.
    fn oldest() -> i64 {
        unix_now().saturating_sub(MAX_AGE.as_secs() as i64)
    }

    /// The quotes that value their coins now: the source's latest answer
    /// was confident, and the price is under a day old.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    fn quotes(&self) -> Quotes {
        let oldest = Self::oldest();
        Quotes(
            self.0
                .iter()
                .filter(|(_, known)| known.values(oldest))
                .map(|(coin, known)| (*coin, known.quote.clone()))
                .collect(),
        )
    }
}

/// A price source, the chains it last listed, and the coins it priced with
/// confidence. The one stats computation running at a time asks it, so it
/// has one writer.
pub struct Prices {
    source: PriceSource,
    endpoints: Endpoints,
    client: reqwest::Client,
    chains: Mutex<ChainNames>,
    book: Mutex<Book>,
}

impl Prices {
    /// Prices from `source`, at its own endpoints.
    pub fn new(source: PriceSource) -> Self {
        Self::at(source, source.endpoints())
    }

    /// Prices from a server on `base` that answers as `source` does.
    #[cfg(feature = "test-helpers")]
    pub fn mock(source: PriceSource, base: &Url) -> Self {
        Self::at(source, source.endpoints_under(base))
    }

    fn at(source: PriceSource, endpoints: Endpoints) -> Self {
        Self {
            source,
            endpoints,
            client: reqwest::Client::new(),
            chains: Mutex::default(),
            book: Mutex::default(),
        }
    }

    /// The coins priced with confidence before and kept, which stay among
    /// those asked whatever their latest answer: a token anybody wrote
    /// cannot push them out.
    pub(crate) fn priced_before(&self) -> Vec<ListedAddress> {
        let book = self.book();
        let coins = book.0.keys().copied().collect();
        drop(book);
        coins
    }

    /// The chains the source lists now; when the list fails, the last list
    /// it gave, and before any, none.
    async fn chains(&self) -> ChainNames {
        let listed = self
            .source
            .chain_names(&self.client, &self.endpoints.chains)
            .await;
        let mut chains = self.held_chains();
        match listed {
            Ok(listed) => *chains = listed,
            Err(error) => {
                tracing::warn!(%error, "the chain list failed; the last one names the chains")
            }
        }
        let names = chains.clone();
        drop(chains);
        names
    }

    /// Prices for `candidates`, asked now on the chains the source lists.
    /// A coin the source prices with confidence takes the new quote, and
    /// one it answers otherwise is not valued until it is again; a request
    /// that fails keeps the quotes of its coins. A quote older than a day
    /// values nothing. Each listed chain keeps the [`KEPT_PER_CHAIN`] priced
    /// coins that moved the most USD; the rest, and every coin of a chain
    /// the list no longer names, are forgotten and not valued.
    pub(crate) async fn quotes(&self, candidates: &[Candidate]) -> Quotes {
        let chains = self.chains().await;
        let ids: HashMap<String, ListedAddress> = candidates
            .iter()
            .filter_map(|candidate| {
                Some((
                    self.source.coin_id(&chains, &candidate.coin)?,
                    candidate.coin,
                ))
            })
            .collect();
        let mut asked: Vec<&String> = ids.keys().collect();
        asked.sort();
        let mut requests = JoinSet::new();
        for batch in asked.chunks(BATCH) {
            let url = self.source.price_url(&self.endpoints.prices, batch);
            let (source, client) = (self.source, self.client.clone());
            requests.spawn(async move { source.prices(client, url?).await });
        }
        let mut fresh = HashMap::new();
        while let Some(answer) = requests.join_next().await {
            match answer {
                Ok(Ok(answer)) => fresh.extend(answer),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "a price request failed; its coins keep their last prices")
                }
                Err(stopped) => {
                    tracing::warn!(%stopped, "a price request stopped; its coins keep their last prices")
                }
            }
        }
        let mut book = self.book();
        for (id, quote) in fresh {
            if let Some(coin) = ids.get(&id) {
                book.learn(*coin, quote);
            }
        }
        book.keep(
            candidates
                .iter()
                .filter(|candidate| chains.name(candidate.coin.chain_id).is_some()),
        );
        let quotes = book.quotes();
        drop(book);
        quotes
    }

    fn held_chains(&self) -> MutexGuard<'_, ChainNames> {
        self.chains
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A client asking a price source.
trait Fetch {
    /// One request, its answer read up to [`MAX_BODY`] and parsed.
    fn fetch<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> impl Future<Output = Result<T, PriceFetchError>> + Send;
}

impl Fetch for reqwest::Client {
    async fn fetch<T: DeserializeOwned>(&self, url: Url) -> Result<T, PriceFetchError> {
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

/// Why a request to the price source gave nothing.
#[derive(Debug, thiserror::Error)]
enum PriceFetchError {
    #[error("an endpoint is not a URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("{0} cannot hold a path")]
    Base(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("the answer is past {MAX_BODY} bytes")]
    TooLarge,
    #[error("the answer does not parse: {0}")]
    Shape(#[from] serde_json::Error),
}

/// Unix seconds now.
#[cfg_attr(feature = "test-helpers", visibility::make(pub))]
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

impl TokenValue {
    /// `amount` token units valued at `quote`; unpriced without one, or
    /// when the arithmetic overflows.
    pub(crate) fn of(amount: U512, quote: Option<&Quote>) -> Self {
        quote.map_or_else(Self::default, |quote| quote.token_value(amount))
    }
}
