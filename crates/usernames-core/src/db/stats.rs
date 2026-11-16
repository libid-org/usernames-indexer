//! What `/v1/stats` and `/v1/stats/days` read, from one snapshot of the
//! store: the handles that resolve now, and the rollups of the journal,
//! over every day or on a page of days. Every rollup row reads back as one
//! [`CountRow`], its [`Measure`] telling what it counts.

use std::{
    fmt,
    future::Future,
    iter,
    num::ParseIntError,
    str::FromStr,
    time::{
        Duration,
        Instant,
    },
};

use alloy::primitives::{
    hex::FromHexError,
    Address,
    B256,
    U512,
};
use sqlx::PgConnection;

use super::{
    page::{
        Cursor,
        Page,
        Rows,
    },
    sql,
    InvalidCursor,
    Store,
};
use crate::chain::NATIVE_COIN;

/// Numbers the stats routes serve, as the store reads them: totals are the
/// rows with no day, a page of days the rows with one.
pub struct StatsSnapshot {
    /// When the snapshot was taken, in unix seconds by the database's clock.
    pub taken_at: i64,
    /// Every chain an indexer prepared, by id.
    pub chains: Vec<i64>,
    /// The handles that resolve now, per chain and platform.
    pub handles: Vec<HandlesRow>,
    /// The rollups, per chain, measure and key, over every day or per day.
    pub counts: Vec<CountRow>,
    /// The fee receivers the fees were read by.
    pub fee_receivers: FeeReceivers,
    /// The referral shares each chain's escrow holds now; none on a page
    /// of days.
    pub held_shares: Vec<HeldSharesRow>,
}

/// The handles one platform has on one chain that resolve now.
#[derive(sqlx::FromRow)]
pub struct HandlesRow {
    /// The chain.
    pub chain_id: i64,
    /// The platform.
    pub platform_id: B256,
    /// Handles with a holder.
    pub live: i64,
    /// Distinct wallets holding them.
    pub holders: i64,
}

/// What one measure counted under one key on one chain, on one UTC day or
/// over every day.
#[derive(sqlx::FromRow)]
pub struct CountRow {
    /// The chain.
    pub chain_id: i64,
    /// What the row counts.
    #[sqlx(try_from = "String")]
    pub measure: Measure,
    /// The platform id, receiver or token the row counts under; empty for
    /// none, and for every receiver or token together.
    pub key: Vec<u8>,
    /// The day's start in unix seconds; `None` over every day.
    pub day: Option<i64>,
    /// Journal rows counted.
    pub count: i64,
    /// What they moved, as Postgres prints a `numeric`.
    pub amount: String,
    /// Distinct wallets that made them.
    pub wallets: i64,
    /// Of them, those seen under the measure and key for the first time;
    /// all of them over every day.
    pub new_wallets: i64,
}

impl CountRow {
    /// The amount as a number. `None` only for text that is not an unsigned
    /// integer below 2^512, which a sum of the journal's `uint256` amounts
    /// never is.
    pub fn amount(&self) -> Option<U512> {
        self.amount.parse().ok()
    }
}

/// What a rollup row counts. The rollups store its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// The chain's journal rows, and the wallets that linked, sent through
    /// the escrow, or claimed from it.
    Users,
    /// The chain's share of those wallets across every chain: the ones its
    /// folds saw first on a day, or first at all.
    AllChainsUsers,
    /// Bindings per platform id, and the wallets they bound to.
    Links,
    /// Registration fees per receiver, and every receiver's together.
    Fees,
    /// Escrow movements of one flow per token, and of every token together.
    Flow(Flow),
    /// Paid referral shares, by where the escrow put them at the bind.
    Shared(Share),
}

/// A stored measure name this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown measure {0:?}")]
pub struct UnknownMeasure(String);

impl Measure {
    /// Every measure, so a stored name parses back by the same table.
    pub const ALL: [Self; 11] = [
        Self::Users,
        Self::AllChainsUsers,
        Self::Links,
        Self::Fees,
        Self::Flow(Flow::Deposited),
        Self::Flow(Flow::Forwarded),
        Self::Flow(Flow::Claimed),
        Self::Flow(Flow::Refunded),
        Self::Flow(Flow::Onboarding),
        Self::Shared(Share::Forwarded),
        Self::Shared(Share::Escrowed),
    ];

    /// The name the rollups store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::AllChainsUsers => "all_chains_users",
            Self::Links => "links",
            Self::Fees => "fees",
            Self::Flow(flow) => flow.as_str(),
            Self::Shared(share) => share.as_str(),
        }
    }

    /// Which keys a read takes of the measure. Anybody can write a token or
    /// name a fee receiver, so a flow is read by its chain's coin and the
    /// listed tokens, fees by the listed receivers, and both by every key
    /// together.
    fn read(self, listing: &Listing) -> KeyRead<'_> {
        match self {
            Self::Users | Self::AllChainsUsers | Self::Links | Self::Shared(_) => {
                KeyRead::Every
            }
            Self::Fees => KeyRead::Named {
                fixed: &[],
                listed: &listing.fee_receivers,
            },
            Self::Flow(_) => KeyRead::Named {
                fixed: &[NATIVE_COIN],
                listed: &listing.tokens,
            },
        }
    }
}

/// The keys a read takes of one measure.
enum KeyRead<'a> {
    /// Every key the measure counted.
    Every,
    /// Every key together (no key), the keys `fixed`, and the entries of
    /// `listed` on the chain read.
    Named {
        fixed: &'static [Address],
        listed: &'a [ListedAddress],
    },
}

impl FromStr for Measure {
    type Err = UnknownMeasure;

    fn from_str(name: &str) -> Result<Self, UnknownMeasure> {
        Self::ALL
            .into_iter()
            .find(|measure| measure.as_str() == name)
            .ok_or_else(|| UnknownMeasure(name.to_string()))
    }
}

impl TryFrom<String> for Measure {
    type Error = UnknownMeasure;

    fn try_from(name: String) -> Result<Self, UnknownMeasure> {
        name.parse()
    }
}

/// What a read takes: every key of some measures, and of the others the
/// keys each chain names, as chain, measure and key, pairwise.
#[derive(Default)]
pub struct Keys {
    /// The measures read under every key.
    pub every: Vec<&'static str>,
    /// The chain of each pair.
    pub chains: Vec<i64>,
    /// The measure of each pair.
    pub measures: Vec<&'static str>,
    /// The key of each pair.
    pub keys: Vec<Vec<u8>>,
}

impl Keys {
    /// The keys a read of `measures` on `chains` takes under `listing`.
    pub fn of(
        chains: &[i64],
        measures: impl IntoIterator<Item = Measure>,
        listing: &Listing,
    ) -> Self {
        let mut read = Self::default();
        for measure in measures {
            let (fixed, listed) = match measure.read(listing) {
                KeyRead::Every => {
                    read.every.push(measure.as_str());
                    continue;
                }
                KeyRead::Named { fixed, listed } => (fixed, listed),
            };
            for &chain_id in chains {
                let on_chain = listed
                    .iter()
                    .filter(|listed| listed.chain_id == chain_id)
                    .map(|listed| listed.address);
                let named = fixed.iter().copied().chain(on_chain);
                for key in iter::once(Vec::new()).chain(named.map(|key| key.to_vec())) {
                    read.chains.push(chain_id);
                    read.measures.push(measure.as_str());
                    read.keys.push(key);
                }
            }
        }
        read
    }
}

/// The addresses `/v1/stats` names per chain; every other one counts under
/// `other`.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    /// Tokens, beside each chain's own coin: those priced, for a read of
    /// the rollups, and those priced before, kept among the tokens asked a
    /// price for.
    pub tokens: Vec<ListedAddress>,
    /// Registration fee receivers.
    pub fee_receivers: Vec<ListedAddress>,
}

/// An address `/v1/stats` names on one chain, a token or a fee receiver,
/// written `<chain id>:<address>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, sqlx::FromRow)]
pub struct ListedAddress {
    /// The chain.
    pub chain_id: i64,
    /// The address there.
    pub address: Address,
}

/// Why a listed address does not parse.
#[derive(Debug, thiserror::Error)]
pub enum ListedAddressError {
    /// No `:` between the chain id and the address.
    #[error("{0:?} is not <chain id>:<address>")]
    Shape(String),
    /// The chain id is not a number.
    #[error("chain id {0:?}: {1}")]
    Chain(String, ParseIntError),
    /// The address is not one.
    #[error("address {0:?}: {1}")]
    Address(String, FromHexError),
}

impl FromStr for ListedAddress {
    type Err = ListedAddressError;

    fn from_str(listed: &str) -> Result<Self, ListedAddressError> {
        let (chain, address) = listed
            .split_once(':')
            .ok_or_else(|| ListedAddressError::Shape(listed.to_string()))?;
        Ok(Self {
            chain_id: chain
                .parse()
                .map_err(|e| ListedAddressError::Chain(chain.to_string(), e))?,
            address: address
                .parse()
                .map_err(|e| ListedAddressError::Address(address.to_string(), e))?,
        })
    }
}

impl fmt::Display for ListedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.chain_id, self.address)
    }
}

impl ListedAddress {
    /// `listed` as the two arrays a statement unnests into pairs.
    pub fn columns(listed: &[Self]) -> (Vec<i64>, Vec<Vec<u8>>) {
        listed
            .iter()
            .map(|listed| (listed.chain_id, listed.address.to_vec()))
            .unzip()
    }
}

/// A token worth a price, and what its deposits and forwards moved.
#[derive(Debug, sqlx::FromRow)]
pub struct Candidate {
    /// The token.
    #[sqlx(flatten)]
    pub coin: ListedAddress,
    /// The sum of its deposits and forwards, as Postgres prints a
    /// `numeric`.
    pub moved: String,
}

impl Candidate {
    /// The sum as a number, under [`CountRow::amount`]'s rule.
    pub fn moved(&self) -> Option<U512> {
        self.moved.parse().ok()
    }
}

/// The tokens a computation names, once it has their prices.
pub trait NamedTokens {
    /// The tokens named beside each chain's coin.
    fn named(&self) -> Vec<ListedAddress>;
}

/// The tokens per chain asked a price for beyond its coin and those priced
/// before, the most moved first.
pub const CANDIDATES_PER_CHAIN: i64 = 40;

/// An escrow flow the rollups count: one kind of movement, or the claims
/// that brought their holder in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// `Deposited`.
    Deposited,
    /// `Forwarded`.
    Forwarded,
    /// `Claimed`.
    Claimed,
    /// `Refunded`.
    Refunded,
    /// Claims of deposits made while nobody held the handle, by the holder
    /// who bound it after them.
    Onboarding,
}

impl Flow {
    /// Every flow.
    pub const ALL: [Self; 5] = [
        Self::Deposited,
        Self::Forwarded,
        Self::Claimed,
        Self::Refunded,
        Self::Onboarding,
    ];

    /// The name the rollups store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deposited => "deposited",
            Self::Forwarded => "forwarded",
            Self::Claimed => "claimed",
            Self::Refunded => "refunded",
            Self::Onboarding => "onboarding",
        }
    }
}

/// Where the escrow put a paid referral share when the splitter deposited
/// it at the bind. A held share stays escrowed once its holder claims it,
/// so a past day never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Share {
    /// Paid straight to the referrer's holder.
    Forwarded,
    /// Held for the referrer's node, claimed since or not.
    Escrowed,
}

impl Share {
    /// The name the rollups store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forwarded => "shared_forwarded",
            Self::Escrowed => "shared_escrowed",
        }
    }
}

/// The fee receivers a read of the fees names on each chain: the listed
/// ones, which `registrationFees` names, and each chain's referral
/// splitter, whose fees are collected whether listed or not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeeReceivers {
    /// The receivers the deployment lists.
    pub listed: Vec<ListedAddress>,
    /// Each chain's referral splitter, on the chains that watch one.
    pub splitters: Vec<ListedAddress>,
}

impl FeeReceivers {
    /// Whether the deployment lists `receiver`.
    pub fn lists(&self, receiver: &ListedAddress) -> bool {
        self.listed.contains(receiver)
    }

    /// Whether fees paid to `receiver` are collected: it is listed, or its
    /// chain's referral splitter.
    pub fn collects(&self, receiver: &ListedAddress) -> bool {
        self.lists(receiver) || self.splitters.contains(receiver)
    }

    /// Every receiver a read names, each once.
    fn read(&self) -> Vec<ListedAddress> {
        let mut read = self.listed.clone();
        read.extend(
            self.splitters
                .iter()
                .filter(|splitter| !self.lists(splitter)),
        );
        read
    }
}

/// The referral shares one chain's escrow holds now, unclaimed.
#[derive(Debug, sqlx::FromRow)]
pub struct HeldSharesRow {
    /// The chain.
    pub chain_id: i64,
    /// Shares held.
    pub count: i64,
    /// Their sum in the chain's coin, as Postgres prints a `numeric`.
    pub amount: String,
}

impl HeldSharesRow {
    /// The sum as a number, under [`CountRow::amount`]'s rule.
    pub fn amount(&self) -> Option<U512> {
        self.amount.parse().ok()
    }
}

impl Store {
    /// Every number `/v1/stats` serves, read in one snapshot so they agree
    /// with each other. The snapshot first reads the tokens worth a price:
    /// each chain's coin, `listing`'s tokens and the most moved others, up
    /// to [`CANDIDATES_PER_CHAIN`]. `price` values them while the snapshot
    /// counts the live handles. Token flows then name each chain's coin and
    /// the tokens `price` named, fees `listing`'s receivers and each chain's
    /// referral splitter, and both every key together; last come the
    /// referral shares each chain's escrow still holds. The live handles
    /// count the handles table, so a caller runs this once per period, not
    /// per request. The statements share `budget`: the database stops the
    /// one running when it is spent.
    pub async fn stats<F, P>(
        &self,
        listing: &Listing,
        budget: Duration,
        price: impl FnOnce(Vec<Candidate>) -> F,
    ) -> Result<(StatsSnapshot, P), sqlx::Error>
    where
        F: Future<Output = P>,
        P: NamedTokens,
    {
        let deadline = Deadline::after(budget);
        let (kept_chains, kept_tokens) = ListedAddress::columns(&listing.tokens);
        let mut tx = self.pool.begin_with(sql::STATS_SNAPSHOT).await?;
        let taken_at = sqlx::query_scalar(sql::STATS_TAKEN_AT)
            .fetch_one(deadline.bound(&mut tx).await?)
            .await?;
        let chains: Vec<i64> = sqlx::query_scalar(sql::CHAINS)
            .fetch_all(deadline.bound(&mut tx).await?)
            .await?;
        let candidates = sqlx::query_as(sql::STATS_CANDIDATES)
            .bind(NATIVE_COIN.to_vec())
            .bind(&kept_chains)
            .bind(&kept_tokens)
            .bind(CANDIDATES_PER_CHAIN)
            .fetch_all(deadline.bound(&mut tx).await?)
            .await?;
        let (prices, handles) = tokio::join!(price(candidates), async {
            sqlx::query_as(sql::STATS_HANDLES)
                .fetch_all(deadline.bound(&mut tx).await?)
                .await
        });
        let handles = handles?;
        let fee_receivers = FeeReceivers {
            listed: listing.fee_receivers.clone(),
            splitters: sqlx::query_as(sql::STATS_SPLITTERS)
                .fetch_all(deadline.bound(&mut tx).await?)
                .await?,
        };
        let named = Listing {
            tokens: prices.named(),
            fee_receivers: fee_receivers.read(),
        };
        let keys = Keys::of(&chains, Measure::ALL, &named);
        let counts = sqlx::query_as(sql::STATS_TOTALS)
            .bind(&keys.every)
            .bind(&keys.chains)
            .bind(&keys.measures)
            .bind(&keys.keys)
            .fetch_all(deadline.bound(&mut tx).await?)
            .await?;
        let held_shares = sqlx::query_as(sql::STATS_HELD_SHARES)
            .fetch_all(deadline.bound(&mut tx).await?)
            .await?;
        tx.commit().await?;
        let snapshot = StatsSnapshot {
            taken_at,
            chains,
            handles,
            counts,
            fee_receivers,
            held_shares,
        };
        Ok((snapshot, prices))
    }

    /// A page of days, newest first: on one chain with everything the chain
    /// recorded on them, or across every chain with their users. Token
    /// flows name the chain's coin and its listed tokens, fees its listed
    /// receivers and its referral splitter. A page reads its days and then
    /// each day's rows by key: never the journal, and never a token or
    /// receiver nobody named.
    pub async fn stats_days(
        &self,
        page: Page<StatsDay>,
        listing: &Listing,
    ) -> Result<StatsDays, sqlx::Error> {
        let before = page.before.map_or(i64::MAX, |StatsDay(day)| day);
        let mut tx = self.pool.begin_with(sql::STATS_SNAPSHOT).await?;
        let taken_at = sqlx::query_scalar(sql::STATS_TAKEN_AT)
            .fetch_one(&mut *tx)
            .await?;
        let fee_receivers = FeeReceivers {
            listed: listing.fee_receivers.clone(),
            splitters: sqlx::query_as(sql::STATS_SPLITTERS)
                .fetch_all(&mut *tx)
                .await?,
        };
        let named = Listing {
            tokens: listing.tokens.clone(),
            fee_receivers: fee_receivers.read(),
        };
        let (days, chains, keys) = match page.chain {
            Some(chain_id) => {
                let days = sqlx::query_scalar(sql::STATS_CHAIN_DAYS)
                    .bind(chain_id)
                    .bind(before)
                    .bind(page.fetch())
                    .fetch_all(&mut *tx)
                    .await?;
                let measures = Measure::ALL
                    .into_iter()
                    .filter(|measure| *measure != Measure::AllChainsUsers);
                let keys = Keys::of(&[chain_id], measures, &named);
                (days, vec![chain_id], keys)
            }
            None => {
                let days = sqlx::query_scalar(sql::STATS_ALL_DAYS)
                    .bind(before)
                    .bind(page.fetch())
                    .fetch_all(&mut *tx)
                    .await?;
                let chains: Vec<i64> =
                    sqlx::query_scalar(sql::CHAINS).fetch_all(&mut *tx).await?;
                let keys = Keys::of(&chains, [Measure::AllChainsUsers], &named);
                (days, chains, keys)
            }
        };
        let Rows { rows: days, next } = page.split(days);
        let counts = sqlx::query_as(sql::STATS_DAY_COUNTS)
            .bind(&chains)
            .bind(&days)
            .bind(&keys.every)
            .bind(&keys.chains)
            .bind(&keys.measures)
            .bind(&keys.keys)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(StatsDays {
            snapshot: StatsSnapshot {
                taken_at,
                chains,
                handles: Vec::new(),
                counts,
                fee_receivers,
                held_shares: Vec::new(),
            },
            next,
        })
    }
}

/// A page of days as the store read it, and the cursor of the next page.
pub struct StatsDays {
    /// The page's rows.
    pub snapshot: StatsSnapshot,
    /// Where the next page starts; `None` on the last.
    pub next: Option<StatsDay>,
}

/// Where a page of days ends: the start of the last day it served, in unix
/// seconds, the next page's exclusive upper bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsDay(pub i64);

impl Cursor<i64> for StatsDay {
    fn after(day: &i64) -> Option<Self> {
        Some(Self(*day))
    }
}

impl FromStr for StatsDay {
    type Err = InvalidCursor;

    fn from_str(raw: &str) -> Result<Self, InvalidCursor> {
        raw.parse().map(Self).map_err(|_| InvalidCursor)
    }
}

impl fmt::Display for StatsDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// When a computation's time runs out. Its statements share the time: each
/// may run only for what the ones before it left, and the database stops
/// it there, so the work ends on the server too.
pub struct Deadline(Instant);

impl Deadline {
    /// The deadline `budget` from now.
    pub fn after(budget: Duration) -> Self {
        Self(Instant::now() + budget)
    }

    /// `conn`, its next statement bounded by the time left, in whole
    /// milliseconds rounded up, so the database never stops it before the
    /// deadline. A millisecond at least: Postgres reads a timeout of zero as
    /// none.
    pub async fn bound<'c>(
        &self,
        conn: &'c mut PgConnection,
    ) -> Result<&'c mut PgConnection, sqlx::Error> {
        let left = self
            .0
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        sqlx::query(sql::STATS_DEADLINE)
            .bind(left.as_micros().div_ceil(1_000).to_string())
            .execute(&mut *conn)
            .await?;
        Ok(conn)
    }
}
