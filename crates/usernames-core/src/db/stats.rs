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
}

/// A stored measure name this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown measure {0:?}")]
pub struct UnknownMeasure(String);

impl Measure {
    /// Every measure, so a stored name parses back by the same table.
    pub const ALL: [Self; 9] = [
        Self::Users,
        Self::AllChainsUsers,
        Self::Links,
        Self::Fees,
        Self::Flow(Flow::Deposited),
        Self::Flow(Flow::Forwarded),
        Self::Flow(Flow::Claimed),
        Self::Flow(Flow::Refunded),
        Self::Flow(Flow::Onboarding),
    ];

    /// The name the rollups store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::AllChainsUsers => "all_chains_users",
            Self::Links => "links",
            Self::Fees => "fees",
            Self::Flow(flow) => flow.as_str(),
        }
    }

    /// Which keys a read takes of the measure. Anybody can write a token or
    /// name a fee receiver, so a flow is read by its chain's coin and the
    /// listed tokens, fees by the listed receivers, and both by every key
    /// together.
    fn read(self, listing: &Listing) -> KeyRead<'_> {
        match self {
            Self::Users | Self::AllChainsUsers | Self::Links => KeyRead::Every,
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
struct Keys {
    /// The measures read under every key.
    every: Vec<&'static str>,
    chains: Vec<i64>,
    measures: Vec<&'static str>,
    keys: Vec<Vec<u8>>,
}

impl Keys {
    fn of(
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
    fn columns(listed: &[Self]) -> (Vec<i64>, Vec<Vec<u8>>) {
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

impl Store {
    /// Every number `/v1/stats` serves, read in one snapshot so they agree
    /// with each other. The snapshot first reads the tokens worth a price:
    /// each chain's coin, `listing`'s tokens and the most moved others, up
    /// to [`CANDIDATES_PER_CHAIN`]. `price` values them while the snapshot
    /// counts the live handles. Token flows then name each chain's coin and
    /// the tokens `price` named, fees `listing`'s receivers, and both every
    /// key together. The live handles count the handles table, so a caller
    /// runs this once per period, not per request. The statements share
    /// `budget`: the database stops the one running when it is spent.
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
        let named = Listing {
            tokens: prices.named(),
            fee_receivers: listing.fee_receivers.clone(),
        };
        let keys = Keys::of(&chains, Measure::ALL, &named);
        let counts = sqlx::query_as(sql::STATS_TOTALS)
            .bind(&keys.every)
            .bind(&keys.chains)
            .bind(&keys.measures)
            .bind(&keys.keys)
            .fetch_all(deadline.bound(&mut tx).await?)
            .await?;
        tx.commit().await?;
        let snapshot = StatsSnapshot {
            taken_at,
            chains,
            handles,
            counts,
        };
        Ok((snapshot, prices))
    }

    /// A page of days, newest first: on one chain with everything the chain
    /// recorded on them, or across every chain with their users. Token
    /// flows name the chain's coin and its listed tokens, fees its listed
    /// receivers. A page reads its days and then each day's rows by key:
    /// never the journal, and never a token or receiver nobody named.
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
                let keys = Keys::of(&[chain_id], measures, listing);
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
                let keys = Keys::of(&chains, [Measure::AllChainsUsers], listing);
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
struct Deadline(Instant);

impl Deadline {
    /// The deadline `budget` from now.
    fn after(budget: Duration) -> Self {
        Self(Instant::now() + budget)
    }

    /// `conn`, its next statement bounded by the time left, in whole
    /// milliseconds rounded up, so the database never stops it before the
    /// deadline. A millisecond at least: Postgres reads a timeout of zero as
    /// none.
    async fn bound<'c>(
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

#[cfg(test)]
pub(crate) mod plans {
    use sqlx::PgConnection;

    use super::*;
    use crate::db::{
        fold::{
            Fold,
            JournalPosition,
        },
        plan_tests::{
            explain,
            Plan,
        },
    };

    /// The stats reads and the rollups' writes, planned.
    pub(crate) trait StatsPlans {
        /// The totals, every shape of a day page, and the folds of a
        /// window's rows and of a journal's page, with the indexes they
        /// seek.
        async fn stats_plans(&mut self) -> Vec<Plan>;
    }

    impl StatsPlans for PgConnection {
        async fn stats_plans(&mut self) -> Vec<Plan> {
            // A cursor near the newest day, where a page read the wrong way
            // has most of the days still ahead of it.
            let before = 190 * 86_400i64;
            let days: Vec<i64> = (161..190).map(|day| day * 86_400).collect();
            // Every day the volume holds: the longest page it can serve.
            let all_days: Vec<i64> = (1..=200).map(|day| day * 86_400).collect();
            let listed = |byte| {
                vec![ListedAddress {
                    chain_id: 1,
                    address: Address::repeat_byte(byte),
                }]
            };
            let listing = Listing {
                tokens: listed(0xEE),
                fee_receivers: listed(0xFE),
            };
            let chains = [1i64, 2, 3, 4, 5];
            let chain_measures = Measure::ALL
                .into_iter()
                .filter(|measure| *measure != Measure::AllChainsUsers);
            let chain_keys = Keys::of(&[1], chain_measures, &listing);
            let all_chains = Keys::of(&chains, [Measure::AllChainsUsers], &listing);
            let every = Keys::of(&chains, Measure::ALL, &listing);
            let measures: Vec<&str> = Measure::ALL.iter().map(|m| m.as_str()).collect();
            let (blocks, logs) = ([10i64, 20, 30], [0i64, 0, 0]);
            let (after, through) = (
                JournalPosition {
                    block_number: 4_000,
                    log_index: 0,
                },
                JournalPosition {
                    block_number: 4_100,
                    log_index: 0,
                },
            );
            let window = Fold::Window {
                blocks: &blocks,
                logs: &logs,
            };
            let page = Fold::Page { after, through };
            let reads = [
                (
                    "the live handles",
                    &["handles_live_idx"][..],
                    sqlx::query_scalar::<_, String>(&explain(sql::STATS_HANDLES))
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the totals",
                    &["stats_totals_pkey:chain_id,measure"][..],
                    sqlx::query_scalar(&explain(sql::STATS_TOTALS))
                        .bind(&every.every)
                        .bind(&every.chains)
                        .bind(&every.measures)
                        .bind(&every.keys)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the price candidates",
                    &["stats_totals_pkey:chain_id,measure"][..],
                    sqlx::query_scalar(&explain(sql::STATS_CANDIDATES))
                        .bind(NATIVE_COIN.to_vec())
                        .bind(&[1i64][..])
                        .bind(vec![Address::repeat_byte(0xEE).to_vec()])
                        .bind(CANDIDATES_PER_CHAIN)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "a chain's days",
                    &["stats_counts_pkey"][..],
                    sqlx::query_scalar(&explain(sql::STATS_CHAIN_DAYS))
                        .bind(1i64)
                        .bind(before)
                        .bind(31i64)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the days across chains",
                    &["stats_counts_pkey"][..],
                    sqlx::query_scalar(&explain(sql::STATS_ALL_DAYS))
                        .bind(before)
                        .bind(31i64)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "a chain's day page",
                    &["stats_counts_pkey"][..],
                    sqlx::query_scalar(&explain(sql::STATS_DAY_COUNTS))
                        .bind(&[1i64][..])
                        .bind(&days)
                        .bind(&chain_keys.every)
                        .bind(&chain_keys.chains)
                        .bind(&chain_keys.measures)
                        .bind(&chain_keys.keys)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "a chain's longest day page",
                    &["stats_counts_pkey"][..],
                    sqlx::query_scalar(&explain(sql::STATS_DAY_COUNTS))
                        .bind(&[1i64][..])
                        .bind(&all_days)
                        .bind(&chain_keys.every)
                        .bind(&chain_keys.chains)
                        .bind(&chain_keys.measures)
                        .bind(&chain_keys.keys)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "a day page across chains",
                    &["stats_counts_pkey"][..],
                    sqlx::query_scalar(&explain(sql::STATS_DAY_COUNTS))
                        .bind(&chains[..])
                        .bind(&days)
                        .bind(&all_chains.every)
                        .bind(&all_chains.chains)
                        .bind(&all_chains.measures)
                        .bind(&all_chains.keys)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "a journal's next page",
                    &["events_pkey"][..],
                    sqlx::query_scalar(&explain(sql::STATS_PAGE_END))
                        .bind(1i64)
                        .bind(after.block_number)
                        .bind(after.log_index)
                        .bind(10_000i64)
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "a window's fold",
                    &[
                        "events_pkey",
                        "events_first_deposit_idx",
                        "events_binds_idx",
                    ][..],
                    sqlx::query_scalar(&explain(&window.statement()))
                        .bind(1i64)
                        .bind(&measures)
                        .bind(&blocks[..])
                        .bind(&logs[..])
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "a journal page's fold",
                    &[
                        "events_pkey",
                        "events_first_deposit_idx",
                        "events_binds_idx",
                    ][..],
                    sqlx::query_scalar(&explain(&page.statement()))
                        .bind(1i64)
                        .bind(&measures)
                        .bind(after.block_number)
                        .bind(after.log_index)
                        .bind(through.block_number)
                        .bind(through.log_index)
                        .fetch_all(&mut *self)
                        .await,
                ),
            ];
            reads
                .into_iter()
                .map(|(name, seeks, text)| {
                    let text: Vec<String> =
                        text.unwrap_or_else(|e| panic!("{name}: {e}"));
                    Plan {
                        name,
                        seeks: Some(seeks),
                        text: text.join("\n"),
                    }
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use usernames_fixtures::database_url;

    use super::*;

    /// A computation's statements share its deadline: the second gets only
    /// what the first left, and the database stops it there.
    #[tokio::test]
    async fn the_statements_share_one_deadline() {
        let url = database_url();
        let pool = PgPool::connect(&url).await.expect("connect");
        let mut tx = pool.begin().await.expect("transaction");
        let deadline = Deadline::after(Duration::from_secs(1));
        sqlx::query("SELECT pg_sleep(0.6)")
            .execute(deadline.bound(&mut tx).await.expect("bounded"))
            .await
            .expect("the first statement ends in time");
        let stopped = sqlx::query("SELECT pg_sleep(0.6)")
            .execute(deadline.bound(&mut tx).await.expect("bounded"))
            .await
            .expect_err("the second statement outlasts the deadline");
        let code = stopped.as_database_error().and_then(|error| error.code());
        // query_canceled: the statement timeout stopped it on the server.
        assert_eq!(code.as_deref(), Some("57014"), "{stopped}");
    }

    /// A listed address reads from its `<chain id>:<address>` text and
    /// prints back to it; anything else is refused.
    #[test]
    fn a_listed_address_parses_from_its_text() {
        let text = "8453:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
        let listed: ListedAddress = text.parse().expect("a listed address");
        assert_eq!(listed.chain_id, 8453);
        assert_eq!(listed.to_string(), text);
        for refused in [
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "base:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "8453:0x8335",
        ] {
            assert!(refused.parse::<ListedAddress>().is_err(), "{refused}");
        }
    }
}
