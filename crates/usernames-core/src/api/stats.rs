//! `GET /v1/stats`: the store's numbers, computed at most once per
//! [`STATS_TTL`] however many requests arrive.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use alloy::primitives::Address;
use axum::{
    body::Bytes,
    extract::State,
    http::header::CONTENT_TYPE,
    response::{
        IntoResponse,
        Response,
    },
};
use tokio::sync::Mutex;

use super::{
    error::ApiError,
    model::{
        Activity,
        AllChains,
        AllChainsDay,
        ChainStats,
        DayActivity,
        EscrowActivity,
        FeesPaid,
        OtherTokens,
        Payments,
        PlatformHandles,
        PlatformLinks,
        Stats,
        TokenFlow,
        TokenFlows,
    },
    prices::{
        PriceSource,
        Quote,
        Quotes,
    },
    AppState,
};
use crate::{
    db::{
        ChainToken,
        FeesRow,
        HandlesRow,
        LinksRow,
        StatsSnapshot,
        TokenFlowRow,
    },
    events::EventKind,
    nodes::Platform,
};

/// How long one computation is served. The aggregates read whole tables, and
/// a burst of requests must not run them again on a small database.
const STATS_TTL: Duration = Duration::from_secs(5 * 60);

/// How long one computation may take, its statements together. The lock is
/// held through it, and every request behind it waits that long.
const STATS_DEADLINE: Duration = Duration::from_secs(30);

/// `GET /v1/stats` — totals and a daily series per chain, from one
/// snapshot of the store.
pub(crate) async fn stats(State(state): State<AppState>) -> Result<Response, ApiError> {
    let AppState {
        store,
        prices,
        stats,
    } = state;
    let body = stats
        .get(|| async move {
            let kept = prices
                .as_deref()
                .map(PriceSource::priced_before)
                .unwrap_or_default();
            let (snapshot, quotes) = store
                .stats(&kept, STATS_DEADLINE, |candidates| async move {
                    match prices.as_deref() {
                        Some(source) => source.quotes(&candidates).await,
                        None => Quotes::default(),
                    }
                })
                .await?;
            Stats::of(snapshot, &quotes)
        })
        .await?;
    Ok(([(CONTENT_TYPE, "application/json")], body).into_response())
}

/// The latest answer, serialized once and shared by every request.
#[derive(Clone, Default)]
pub(crate) struct StatsCache {
    latest: Arc<Mutex<Option<Computed>>>,
}

/// One computation's answer as served, and when it finished.
struct Computed {
    at: Instant,
    body: Bytes,
}

impl Computed {
    /// `stats` as served from now on.
    fn of(stats: &Stats) -> Result<Self, ApiError> {
        let body = serde_json::to_vec(stats).map_err(ApiError::internal)?;
        Ok(Self {
            at: Instant::now(),
            body: body.into(),
        })
    }

    /// Whether the answer may still be served at `now`.
    fn fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) < STATS_TTL
    }
}

impl StatsCache {
    /// The answer, from `compute` when none younger than [`STATS_TTL`] is
    /// held. The lock is held through the computation, so requests arriving
    /// meanwhile wait for its answer instead of computing again; after a
    /// failure the next of them computes. The computation runs in a task of
    /// its own: a request whose client goes away leaves it running.
    pub(crate) async fn get<F>(
        &self,
        compute: impl FnOnce() -> F,
    ) -> Result<Bytes, ApiError>
    where
        F: Future<Output = Result<Stats, ApiError>> + Send + 'static,
    {
        let mut latest = Arc::clone(&self.latest).lock_owned().await;
        let held = latest
            .as_ref()
            .filter(|computed| computed.fresh(Instant::now()))
            .map(|computed| computed.body.clone());
        if let Some(body) = held {
            drop(latest);
            return Ok(body);
        }
        let computation = compute();
        tokio::spawn(async move {
            let answer = computation
                .await
                .and_then(|stats| Computed::of(&stats))
                .map(|computed| latest.insert(computed).body.clone());
            drop(latest);
            answer
        })
        .await
        .map_err(|stopped| {
            ApiError::internal(format_args!("the stats computation stopped: {stopped}"))
        })?
    }
}

impl Stats {
    /// The answer from the store's aggregates, its tokens valued at
    /// `quotes`: every chain the store holds, each with its totals and its
    /// days.
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
        for row in snapshot.users {
            match (row.chain_id, row.day) {
                (Some(chain_id), Some(day)) => {
                    let day = chains.chain(chain_id).day(day);
                    day.new_users = row.new_users;
                    day.activity.users = row.users;
                }
                (Some(chain_id), None) => chains.chain(chain_id).total.users = row.users,
                (None, Some(day)) => {
                    let all = AllChainsDay {
                        day,
                        users: row.users,
                        new_users: row.new_users,
                    };
                    days.insert(day, all);
                }
                (None, None) => total.users = row.users,
            }
        }
        for row in snapshot.links {
            let links = PlatformLinks::of(&row);
            chains.chain(row.chain_id).span(row.day).links.push(links);
        }
        for row in snapshot.fees {
            let fees = FeesPaid::of(&row)?;
            chains
                .chain(row.chain_id)
                .span(row.day)
                .registration_fees
                .push(fees);
        }
        for row in snapshot.escrow {
            let kind: EventKind = row.kind.parse().map_err(ApiError::internal)?;
            chains
                .chain(row.flow.chain_id)
                .span(row.flow.day)
                .escrow
                .flows(kind)?
                .add(&row.flow, quotes)?;
        }
        for row in snapshot.onboarding_claims {
            chains
                .chain(row.chain_id)
                .span(row.day)
                .onboarding_claims
                .add(&row, quotes)?;
        }
        for tally in chains.0.values() {
            total.payments.add(&tally.total.escrow.deposits);
            total.payments.add(&tally.total.escrow.forwards);
        }
        Ok(Self {
            computed_at: snapshot.taken_at,
            priced_at: quotes.taken_at(),
            total,
            days: days.into_values().collect(),
            chains: chains.0.into_values().map(ChainStats::from).collect(),
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

impl From<Tally> for ChainStats {
    fn from(tally: Tally) -> Self {
        Self {
            chain_id: tally.chain_id,
            handles: tally.handles,
            total: tally.total,
            days: tally.days.into_values().collect(),
        }
    }
}

impl EscrowActivity {
    /// The flows an escrow event of `kind` counts in.
    fn flows(&mut self, kind: EventKind) -> Result<&mut TokenFlows, ApiError> {
        match kind {
            EventKind::Deposited => Ok(&mut self.deposits),
            EventKind::Forwarded => Ok(&mut self.forwards),
            EventKind::Claimed => Ok(&mut self.claims),
            EventKind::Refunded => Ok(&mut self.refunds),
            other => Err(ApiError::internal(format_args!(
                "escrow statistics of a {other} row"
            ))),
        }
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

impl PlatformLinks {
    fn of(row: &LinksRow) -> Self {
        Self {
            platform: Platform::known_of(row.platform_id),
            platform_id: row.platform_id,
            count: row.count,
            holders: row.holders,
        }
    }
}

impl FeesPaid {
    fn of(row: &FeesRow) -> Result<Self, ApiError> {
        let amount = row.amount().ok_or_else(|| {
            ApiError::internal(format_args!(
                "fees to {} on chain {}: amount {:?}",
                row.receiver, row.chain_id, row.amount
            ))
        })?;
        Ok(Self {
            receiver: row.receiver,
            count: row.count,
            amount,
        })
    }
}

impl Payments {
    /// Count the payments of `flows`.
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
    /// Count `row`, valued at `quotes`: under its token, or under `other`
    /// when it names none.
    fn add(&mut self, row: &TokenFlowRow, quotes: &Quotes) -> Result<(), ApiError> {
        match row.token {
            Some(token) => {
                let coin = ChainToken {
                    chain_id: row.chain_id,
                    token,
                };
                let mut flow = TokenFlow::of(token, row, quotes.0.get(&coin))?;
                match flow.usd.map(|usd| self.usd.checked_add(usd)) {
                    Some(Some(usd)) => self.usd = usd,
                    Some(None) => flow.unprice(),
                    None => {}
                }
                self.tokens.push(flow);
            }
            None => {
                self.other = Some(OtherTokens {
                    count: row.count,
                    wallets: row.wallets,
                })
            }
        }
        Ok(())
    }
}

impl TokenFlow {
    /// `row`'s movements of `token`, valued at `quote` when the arithmetic
    /// holds.
    fn of(
        token: Address,
        row: &TokenFlowRow,
        quote: Option<&Quote>,
    ) -> Result<Self, ApiError> {
        let amount = row.amount().ok_or_else(|| {
            ApiError::internal(format_args!(
                "{token} flows on chain {}: amount {:?}",
                row.chain_id, row.amount
            ))
        })?;
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

    fn answer(computed_at: i64) -> Stats {
        Stats {
            computed_at,
            priced_at: None,
            total: AllChains::default(),
            days: Vec::new(),
            chains: Vec::new(),
        }
    }

    /// The store's answer with nothing priced.
    trait Unpriced {
        async fn unpriced(&self, budget: Duration) -> Result<Stats, ApiError>;
    }

    impl Unpriced for Store {
        async fn unpriced(&self, budget: Duration) -> Result<Stats, ApiError> {
            let (snapshot, quotes) = self
                .stats(&[], budget, |_| async { Quotes::default() })
                .await?;
            Stats::of(snapshot, &quotes)
        }
    }

    /// The answer a cached body serves.
    trait Served {
        fn served(self) -> Stats;
    }

    impl Served for Result<Bytes, ApiError> {
        fn served(self) -> Stats {
            let body = self.ok().expect("an answer");
            serde_json::from_slice(&body).expect("a stats answer")
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
            assert_eq!(body.served(), answer(0));
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
        assert_eq!(second.await.served(), answer(0));
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
        assert_eq!(body.served(), answer(1));
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
        // Another session holds the journal every aggregate reads, so the
        // computation waits on it until its deadline.
        let mut holder = pool.begin().await.expect("transaction");
        sqlx::query("LOCK TABLE names.events IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *holder)
            .await
            .expect("the journal locked");
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
        holder.rollback().await.expect("the journal free");
        let served = cache
            .get(|| async move { store.unpriced(budget).await })
            .await;
        assert!(served.is_ok(), "the next request computes again");
    }

    /// An answer is served for five minutes after its computation.
    #[test]
    fn an_answer_is_served_for_five_minutes() {
        let at = Instant::now();
        let computed = Computed {
            at,
            body: Bytes::new(),
        };
        assert!(computed.fresh(at));
        assert!(computed.fresh(at + STATS_TTL - Duration::from_secs(1)));
        assert!(!computed.fresh(at + STATS_TTL));
    }
}
