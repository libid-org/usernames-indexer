//! `GET /v1/stats` and `GET /v1/stats/days`: the store's rollups, valued
//! in USD where a token is priced. The totals are computed at most once per
//! time to live, [`STATS_TTL`] by default, however many requests arrive; a
//! page of days is read per request, its size capped, and names and values
//! tokens as the totals being served do.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use alloy::primitives::{
    Address,
    B256,
    U512,
};
use axum::{
    body::Bytes,
    extract::State,
    http::header::CONTENT_TYPE,
    response::{
        IntoResponse,
        Response,
    },
    Json,
};
use tokio::sync::Mutex;

use super::{
    error::{
        ApiError,
        ApiQuery,
    },
    model::{
        Activity,
        AllChains,
        AllChainsDay,
        ChainStats,
        DayActivity,
        FeesPaid,
        OtherFees,
        OtherTokens,
        Payments,
        PlatformHandles,
        PlatformLinks,
        RegistrationFees,
        Stats,
        StatsDays,
        TokenFlow,
        TokenFlows,
    },
    params::{
        PageParams,
        DAYS_PAGE,
    },
    prices::{
        Prices,
        Quote,
        Quotes,
    },
    AppState,
};
use crate::{
    db::{
        CountRow,
        Flow,
        HandlesRow,
        ListedAddress,
        Listing,
        Measure,
        NamedTokens,
        StatsDay,
        StatsSnapshot,
    },
    nodes::Platform,
};

/// How long one computation of the totals is served by default. They sum
/// every day of every chain and ask the price source, and a burst of
/// requests must not run them again.
pub const STATS_TTL: Duration = Duration::from_secs(5 * 60);

/// How long one computation may take by default, its statements together.
/// The lock is held through it, and every request behind it waits that long.
pub const STATS_DEADLINE: Duration = Duration::from_secs(30);

/// `GET /v1/stats` — totals per chain and across them, from one snapshot of
/// the store.
pub(crate) async fn stats(State(state): State<AppState>) -> Result<Response, ApiError> {
    let served = state.served_stats().await?;
    Ok(([(CONTENT_TYPE, "application/json")], served.body).into_response())
}

/// `GET /v1/stats/days?chain=&before=&limit=` — the daily series, newest
/// first, across every chain or on the one named: the last 30 days unless
/// `limit` asks for more, at most 366.
pub(crate) async fn days(
    State(state): State<AppState>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Response, ApiError> {
    let page = params.page_within::<StatsDay>(DAYS_PAGE)?;
    let chain_id = page.chain;
    let store = state.store.clone();
    let fee_receivers = state.fee_receivers.to_vec();
    let quotes = state.served_stats().await?.quotes;
    let listing = Listing {
        tokens: quotes.named(),
        fee_receivers,
    };
    let read = store.stats_days(page, &listing).await?;
    let next = read.next.map(|day| day.to_string());
    let Report { stats, mut history } = Report::of(read.snapshot, &quotes)?;
    let computed_at = stats.computed_at;
    let priced_at = stats.priced_at;
    Ok(match chain_id {
        None => Json(StatsDays {
            computed_at,
            priced_at,
            chain_id,
            days: history.days.into_iter().rev().collect(),
            next,
        })
        .into_response(),
        Some(chain) => Json(StatsDays {
            computed_at,
            priced_at,
            chain_id,
            days: history
                .chains
                .remove(&chain)
                .unwrap_or_default()
                .into_iter()
                .rev()
                .collect(),
            next,
        })
        .into_response(),
    })
}

impl AppState {
    /// The latest totals, computed when none are fresh.
    async fn served_stats(self) -> Result<Served, ApiError> {
        let AppState {
            store,
            prices,
            fee_receivers,
            stats,
        } = self;
        let deadline = stats.deadline;
        stats
            .get(|| async move {
                let listing = Listing {
                    tokens: prices
                        .as_deref()
                        .map(Prices::priced_before)
                        .unwrap_or_default(),
                    fee_receivers: fee_receivers.to_vec(),
                };
                let (snapshot, quotes) = store
                    .stats(&listing, deadline, |candidates| async move {
                        match prices.as_deref() {
                            Some(source) => source.quotes(&candidates).await,
                            None => Quotes::default(),
                        }
                    })
                    .await?;
                let report = Report::of(snapshot, &quotes)?;
                Ok(Valued {
                    stats: report.stats,
                    quotes,
                })
            })
            .await
    }
}

/// The latest totals, serialized once and shared by every request, served
/// for `ttl`; a computation may take `deadline`.
#[derive(Clone)]
pub(crate) struct StatsCache {
    latest: Arc<Mutex<Option<Computed>>>,
    ttl: Duration,
    deadline: Duration,
}

impl Default for StatsCache {
    fn default() -> Self {
        Self::new(STATS_TTL, STATS_DEADLINE)
    }
}

/// One computation's totals and the prices that valued them.
pub(crate) struct Valued {
    stats: Stats,
    quotes: Quotes,
}

/// The numbers a snapshot holds, as the stats routes serve them: the totals,
/// and the days, oldest first.
pub(crate) struct Report {
    stats: Stats,
    history: History,
}

/// The days of a snapshot, oldest first; a day with nothing recorded is
/// absent.
#[derive(Debug, Default, PartialEq, Eq)]
struct History {
    /// Across every chain.
    days: Vec<AllChainsDay>,
    /// Per chain, by chain id.
    chains: BTreeMap<i64, Vec<DayActivity>>,
}

/// The totals as served: serialized once, with the prices that valued
/// them, which name and value the tokens of a page of days too.
#[derive(Clone)]
pub(crate) struct Served {
    body: Bytes,
    quotes: Arc<Quotes>,
}

/// One computation's answer as served, and when it finished.
struct Computed {
    at: Instant,
    served: Served,
}

impl Computed {
    /// `valued` as served from now on.
    fn of(valued: Valued) -> Result<Self, ApiError> {
        let body = serde_json::to_vec(&valued.stats).map_err(ApiError::internal)?;
        Ok(Self {
            at: Instant::now(),
            served: Served {
                body: body.into(),
                quotes: Arc::new(valued.quotes),
            },
        })
    }

    /// Whether the answer may still be served at `now`, for `ttl` after it
    /// was computed.
    fn fresh(&self, now: Instant, ttl: Duration) -> bool {
        now.saturating_duration_since(self.at) < ttl
    }
}

impl StatsCache {
    /// An empty cache serving each answer for `ttl`, each computation
    /// given `deadline`.
    pub(crate) fn new(ttl: Duration, deadline: Duration) -> Self {
        Self {
            latest: Arc::default(),
            ttl,
            deadline,
        }
    }

    /// The answer, from `compute` when none younger than the cache's `ttl`
    /// is held. The lock is held through the computation, so requests arriving
    /// meanwhile wait for its answer instead of computing again; after a
    /// failure the next of them computes. The computation runs in a task of
    /// its own: a request whose client goes away leaves it running.
    pub(crate) async fn get<F>(
        &self,
        compute: impl FnOnce() -> F,
    ) -> Result<Served, ApiError>
    where
        F: Future<Output = Result<Valued, ApiError>> + Send + 'static,
    {
        let mut latest = Arc::clone(&self.latest).lock_owned().await;
        let held = latest
            .as_ref()
            .filter(|computed| computed.fresh(Instant::now(), self.ttl))
            .map(|computed| computed.served.clone());
        if let Some(served) = held {
            drop(latest);
            return Ok(served);
        }
        let computation = compute();
        tokio::spawn(async move {
            let answer = computation
                .await
                .and_then(Computed::of)
                .map(|computed| latest.insert(computed).served.clone());
            drop(latest);
            answer
        })
        .await
        .map_err(|stopped| {
            ApiError::internal(format_args!("the stats computation stopped: {stopped}"))
        })?
    }
}

impl Report {
    /// The answer from the store's rollups, its tokens valued at `quotes`:
    /// every chain the snapshot read, each with its totals and its days.
    fn of(snapshot: StatsSnapshot, quotes: &Quotes) -> Result<Self, ApiError> {
        let mut chains = Tallies(
            snapshot
                .chains
                .into_iter()
                .map(|chain_id| (chain_id, Tally::new(chain_id)))
                .collect(),
        );
        for row in snapshot.handles {
            chains
                .chain(row.chain_id)
                .handles
                .push(PlatformHandles::of(row));
        }
        let mut total = AllChains::default();
        let mut days = BTreeMap::new();
        for row in &snapshot.counts {
            let chain = chains.chain(row.chain_id);
            match (row.measure, row.day) {
                (Measure::Users, Some(day)) => {
                    let day = chain.day(day);
                    day.new_users = row.new_wallets;
                    day.activity.users = row.wallets;
                }
                (Measure::Users, None) => chain.total.users = row.wallets,
                (Measure::AllChainsUsers, Some(day)) => {
                    let all = days.entry(day).or_insert(AllChainsDay {
                        day,
                        users: 0,
                        new_users: 0,
                    });
                    all.users += row.wallets;
                    all.new_users += row.new_wallets;
                }
                (Measure::AllChainsUsers, None) => total.users += row.wallets,
                (Measure::Links, day) => {
                    chain.span(day).links.push(PlatformLinks::of(row)?)
                }
                (Measure::Fees, day) => chain.span(day).registration_fees.add(row)?,
                (Measure::Flow(flow), day) => {
                    chain.span(day).flows(flow).add(row, quotes)?
                }
            }
        }
        let mut history = History {
            days: days.into_values().collect(),
            chains: BTreeMap::new(),
        };
        let chains = chains
            .0
            .into_values()
            .map(|mut tally| {
                tally.total.settle();
                total.payments.add(&tally.total.escrow.deposits);
                total.payments.add(&tally.total.escrow.forwards);
                let days = tally.days.into_values().map(|mut day| {
                    day.activity.settle();
                    day
                });
                history.chains.insert(tally.chain_id, days.collect());
                ChainStats {
                    chain_id: tally.chain_id,
                    handles: tally.handles,
                    total: tally.total,
                }
            })
            .collect();
        Ok(Self {
            stats: Stats {
                computed_at: snapshot.taken_at,
                priced_at: quotes.taken_at(),
                total,
                chains,
            },
            history,
        })
    }
}

/// Every chain's numbers while the rows come in, by chain id.
struct Tallies(BTreeMap<i64, Tally>);

impl Tallies {
    /// The chain's numbers; a row of a chain the snapshot did not list
    /// still counts.
    fn chain(&mut self, chain_id: i64) -> &mut Tally {
        self.0
            .entry(chain_id)
            .or_insert_with(|| Tally::new(chain_id))
    }
}

/// One chain's numbers while the rows come in.
struct Tally {
    chain_id: i64,
    handles: Vec<PlatformHandles>,
    total: Activity,
    days: BTreeMap<i64, DayActivity>,
}

impl Tally {
    fn new(chain_id: i64) -> Self {
        Self {
            chain_id,
            handles: Vec::new(),
            total: Activity::default(),
            days: BTreeMap::new(),
        }
    }

    /// The day starting at `day`, in unix seconds.
    fn day(&mut self, day: i64) -> &mut DayActivity {
        self.days.entry(day).or_insert_with(|| DayActivity {
            day,
            new_users: 0,
            activity: Activity::default(),
        })
    }

    /// What a row adds to: its day's activity, or the total when it names
    /// no day.
    fn span(&mut self, day: Option<i64>) -> &mut Activity {
        match day {
            Some(day) => &mut self.day(day).activity,
            None => &mut self.total,
        }
    }
}

impl Activity {
    /// The flows a row of `flow` counts in.
    fn flows(&mut self, flow: Flow) -> &mut TokenFlows {
        match flow {
            Flow::Deposited => &mut self.escrow.deposits,
            Flow::Forwarded => &mut self.escrow.forwards,
            Flow::Claimed => &mut self.escrow.claims,
            Flow::Refunded => &mut self.escrow.refunds,
            Flow::Onboarding => &mut self.onboarding_claims,
        }
    }

    /// Once every row is in: `other` only where the tokens or receivers
    /// nobody named moved or were paid.
    fn settle(&mut self) {
        for flow in Flow::ALL {
            let flows = self.flows(flow);
            flows.other = flows.other.take().filter(|other| other.count > 0);
        }
        self.registration_fees.settle();
    }
}

impl PlatformHandles {
    fn of(row: HandlesRow) -> Self {
        Self {
            platform: Platform::known_of(row.platform_id),
            platform_id: row.platform_id,
            live: row.live,
            holders: row.holders,
        }
    }
}

/// A rollup row's parts, as the wire names them.
trait Parts {
    /// The key, as the type the measure keys by.
    fn key_as<K: for<'a> TryFrom<&'a [u8]>>(&self) -> Result<K, ApiError>;
    /// The amount.
    fn amount_of(&self) -> Result<U512, ApiError>;
}

impl Parts for CountRow {
    fn key_as<K: for<'a> TryFrom<&'a [u8]>>(&self) -> Result<K, ApiError> {
        K::try_from(&self.key).map_err(|_| {
            ApiError::internal(format_args!(
                "{} on chain {}: key 0x{}",
                self.measure.as_str(),
                self.chain_id,
                alloy::hex::encode(&self.key)
            ))
        })
    }

    fn amount_of(&self) -> Result<U512, ApiError> {
        self.amount().ok_or_else(|| {
            ApiError::internal(format_args!(
                "{} on chain {}: amount {:?}",
                self.measure.as_str(),
                self.chain_id,
                self.amount
            ))
        })
    }
}

impl PlatformLinks {
    fn of(row: &CountRow) -> Result<Self, ApiError> {
        let platform_id: B256 = row.key_as()?;
        Ok(Self {
            platform: Platform::known_of(platform_id),
            platform_id,
            count: row.count,
            holders: row.wallets,
        })
    }
}

impl FeesPaid {
    fn of(row: &CountRow) -> Result<Self, ApiError> {
        Ok(Self {
            receiver: row.key_as()?,
            count: row.count,
            amount: row.amount_of()?,
        })
    }
}

impl RegistrationFees {
    /// Count `row`: under its receiver, or, when it names none, as every
    /// receiver's fees, which `other` holds until `settle`.
    fn add(&mut self, row: &CountRow) -> Result<(), ApiError> {
        match row.key.is_empty() {
            true => {
                self.other = Some(OtherFees {
                    count: row.count,
                    amount: row.amount_of()?,
                })
            }
            false => self.receivers.push(FeesPaid::of(row)?),
        }
        Ok(())
    }

    /// Once every row is in: `other` holds every receiver's fees less the
    /// listed receivers', and only when there were any. The listed rows are
    /// part of every receiver's, from the same snapshot, so neither
    /// difference goes below zero.
    fn settle(&mut self) {
        let (count, amount) =
            self.receivers
                .iter()
                .fold((0, U512::ZERO), |(count, amount), paid| {
                    (count + paid.count, amount.saturating_add(paid.amount))
                });
        self.other = self
            .other
            .take()
            .map(|every| OtherFees {
                count: every.count - count,
                amount: every.amount.saturating_sub(amount),
            })
            .filter(|other| other.count > 0);
    }
}

impl Payments {
    /// Count the payments of `flows`, once every row is in.
    fn add(&mut self, flows: &TokenFlows) {
        for flow in &flows.tokens {
            match flow.usd.and_then(|usd| self.usd.checked_add(usd)) {
                Some(usd) => self.usd = usd,
                None => self.unpriced += flow.count,
            }
        }
        self.unpriced += flows.other.as_ref().map_or(0, |other| other.count);
    }
}

impl TokenFlows {
    /// Count `row`, valued at `quotes`: under its token, or, when it names
    /// none, as every token's movements, which `other` holds less the named
    /// tokens'.
    fn add(&mut self, row: &CountRow, quotes: &Quotes) -> Result<(), ApiError> {
        let unnamed = match row.key.is_empty() {
            true => row.count,
            false => {
                let token: Address = row.key_as()?;
                let coin = ListedAddress {
                    chain_id: row.chain_id,
                    address: token,
                };
                let mut flow = TokenFlow::of(token, row, quotes.0.get(&coin))?;
                match flow.usd.map(|usd| self.usd.checked_add(usd)) {
                    Some(Some(usd)) => self.usd = usd,
                    Some(None) => flow.unprice(),
                    None => {}
                }
                self.tokens.push(flow);
                -row.count
            }
        };
        self.other.get_or_insert(OtherTokens { count: 0 }).count += unnamed;
        Ok(())
    }
}

impl TokenFlow {
    /// `row`'s movements of `token`, valued at `quote` when the arithmetic
    /// holds.
    fn of(
        token: Address,
        row: &CountRow,
        quote: Option<&Quote>,
    ) -> Result<Self, ApiError> {
        let amount = row.amount_of()?;
        let usd = quote.and_then(|quote| quote.usd(amount));
        let quote = quote.filter(|_| usd.is_some());
        Ok(Self {
            token,
            count: row.count,
            amount,
            wallets: row.wallets,
            symbol: quote.and_then(|quote| quote.symbol.clone()),
            decimals: quote.map(|quote| quote.decimals),
            value: quote.map(|quote| quote.value(amount)),
            usd,
        })
    }

    /// The flow without a price, as when its sum would overflow.
    fn unprice(&mut self) {
        self.symbol = None;
        self.decimals = None;
        self.value = None;
        self.usd = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{
        AtomicI64,
        Ordering,
    };

    use futures_util::future::join_all;
    use sqlx::PgPool;
    use tokio::sync::oneshot;
    use usernames_fixtures::database_url;

    use super::*;
    use crate::db::{
        Store,
        MIGRATOR,
    };

    fn stats(computed_at: i64) -> Stats {
        Stats {
            computed_at,
            priced_at: None,
            total: AllChains::default(),
            chains: Vec::new(),
        }
    }

    /// A computation's answer with nothing priced.
    fn answer(computed_at: i64) -> Valued {
        Valued {
            stats: stats(computed_at),
            quotes: Quotes::default(),
        }
    }

    /// The store's answer with nothing priced.
    trait Unpriced {
        async fn unpriced(&self, budget: Duration) -> Result<Valued, ApiError>;
    }

    impl Unpriced for Store {
        async fn unpriced(&self, budget: Duration) -> Result<Valued, ApiError> {
            let (snapshot, quotes) = self
                .stats(&Listing::default(), budget, |_| async { Quotes::default() })
                .await?;
            let stats = Report::of(snapshot, &quotes)?.stats;
            Ok(Valued { stats, quotes })
        }
    }

    /// The totals a cached answer serves.
    trait Totals {
        fn served(self) -> Stats;
    }

    impl Totals for Result<Served, ApiError> {
        fn served(self) -> Stats {
            let served = self.ok().expect("an answer");
            serde_json::from_slice(&served.body).expect("a stats answer")
        }
    }

    /// Ten requests that arrive together run one computation, and each gets
    /// its answer.
    #[tokio::test]
    async fn requests_arriving_together_share_one_computation() {
        let cache = StatsCache::default();
        let computations = Arc::new(AtomicI64::new(0));
        let answers = join_all((0..10).map(|_| {
            let computations = Arc::clone(&computations);
            cache.get(|| async move {
                let computation = computations.fetch_add(1, Ordering::SeqCst);
                // The other requests are polled while this one computes.
                tokio::task::yield_now().await;
                Ok(answer(computation))
            })
        }))
        .await;
        assert_eq!(computations.load(Ordering::SeqCst), 1);
        for body in answers {
            assert_eq!(body.served(), stats(0));
        }
    }

    /// A request whose client goes away mid-computation leaves the
    /// computation running, and the request behind it gets its answer.
    #[tokio::test]
    async fn a_dropped_request_leaves_its_computation_running() {
        let cache = StatsCache::default();
        let (started, has_started) = oneshot::channel();
        let (finish, finishes) = oneshot::channel();
        let first = cache.get(|| async move {
            started.send(()).expect("the test waits for the start");
            finishes.await.expect("the test lets it finish");
            Ok(answer(0))
        });
        // axum drops a request's future when its client disconnects.
        tokio::select! {
            _ = first => panic!("the computation finished before it was let go"),
            started = has_started => started.expect("the computation started"),
        }
        let second = cache.get(|| async { Ok(answer(1)) });
        finish.send(()).expect("the computation still runs");
        assert_eq!(second.await.served(), stats(0));
    }

    /// A failed computation is not served: the next request computes.
    #[tokio::test]
    async fn a_failed_computation_is_computed_again() {
        let cache = StatsCache::default();
        let failed = cache
            .get(|| async { Err(ApiError::internal("the database is down")) })
            .await;
        assert!(failed.is_err());
        let body = cache.get(|| async { Ok(answer(1)) }).await;
        assert_eq!(body.served(), stats(1));
    }

    /// A computation past its deadline fails on the database and is not
    /// served: once the journal is free, the next request computes again.
    #[tokio::test]
    async fn a_computation_past_its_deadline_is_not_served() {
        let Some(url) = database_url() else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let pool = PgPool::connect(&url).await.expect("connect");
        MIGRATOR.run(&pool).await.expect("migrations");
        let store = Store::new(pool.clone());
        let budget = Duration::from_secs(1);
        // Another session holds the users' rollup the totals read, so the
        // computation waits on it until its deadline.
        let mut holder = pool.begin().await.expect("transaction");
        sqlx::query("LOCK TABLE names.stats_totals IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *holder)
            .await
            .expect("the rollup locked");
        let cache = StatsCache::default();
        let waiting = store.clone();
        let started = Instant::now();
        let failed = cache
            .get(|| async move { waiting.unpriced(budget).await })
            .await;
        let waited = started.elapsed();
        assert!(failed.is_err(), "the computation outlasted its deadline");
        assert!(
            waited >= budget && waited < 2 * budget,
            "stopped after {waited:?}"
        );
        holder.rollback().await.expect("the rollup free");
        let served = cache
            .get(|| async move { store.unpriced(budget).await })
            .await;
        assert!(served.is_ok(), "the next request computes again");
    }

    /// An answer is served for its time to live after its computation.
    #[test]
    fn an_answer_is_served_for_its_time_to_live() {
        let at = Instant::now();
        let computed = Computed {
            at,
            served: Served {
                body: Bytes::new(),
                quotes: Arc::default(),
            },
        };
        let ttl = Duration::from_secs(90);
        assert!(computed.fresh(at, ttl));
        assert!(computed.fresh(at + ttl - Duration::from_secs(1), ttl));
        assert!(!computed.fresh(at + ttl, ttl));
    }

    /// A cache serves for the time to live it was given: with none, every
    /// request computes.
    #[tokio::test]
    async fn a_cache_serves_for_the_time_to_live_it_was_given() {
        let cache = StatsCache::new(Duration::ZERO, STATS_DEADLINE);
        let first = cache.get(|| async { Ok(answer(0)) }).await;
        assert_eq!(first.served(), stats(0));
        let next = cache.get(|| async { Ok(answer(1)) }).await;
        assert_eq!(next.served(), stats(1));
    }
}
