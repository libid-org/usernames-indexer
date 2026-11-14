//! `GET /v1/stats` and `GET /v1/stats/days`: the store's numbers, computed
//! at most once per [`STATS_TTL`] however many requests arrive. The totals
//! are one body; the daily series is held beside it and served a page at a
//! time.

use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    str::FromStr,
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
        EscrowActivity,
        FeesPaid,
        OtherTokens,
        PlatformHandles,
        PlatformLinks,
        Stats,
        StatsDays,
        TokenFlow,
        TokenFlows,
    },
    params::PageParams,
    AppState,
};
use crate::{
    db::{
        Cursor,
        FeesRow,
        HandlesRow,
        InvalidCursor,
        LinksRow,
        Page,
        Rows,
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

/// `GET /v1/stats` — totals per chain and across them, from one snapshot of
/// the store.
pub(crate) async fn stats(State(state): State<AppState>) -> Result<Response, ApiError> {
    let served = state.served_stats().await?;
    Ok(([(CONTENT_TYPE, "application/json")], served.body).into_response())
}

/// `GET /v1/stats/days?chain=&before=&limit=` — the daily series, newest
/// first, across every chain or on the one named, from the snapshot
/// `/v1/stats` serves.
pub(crate) async fn days(
    State(state): State<AppState>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Response, ApiError> {
    let page = params.page::<Day>()?;
    let served = state.served_stats().await?;
    Ok(match page.chain {
        None => Json(served.page(&served.history.days, page)).into_response(),
        Some(chain_id) => {
            let days = served
                .history
                .chains
                .get(&chain_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            Json(served.page(days, page)).into_response()
        }
    })
}

impl AppState {
    /// The latest answer, computed when none is fresh.
    async fn served_stats(self) -> Result<Served, ApiError> {
        let AppState {
            store,
            listed,
            stats,
        } = self;
        stats
            .get(
                || async move { Report::of(store.stats(&listed, STATS_DEADLINE).await?) },
            )
            .await
    }
}

/// The latest answer, serialized once and shared by every request.
#[derive(Clone, Default)]
pub(crate) struct StatsCache {
    latest: Arc<Mutex<Option<Computed>>>,
}

/// One computation as the stats routes read it: the totals, and the daily
/// series beside them.
pub(crate) struct Report {
    stats: Stats,
    history: History,
}

/// The daily series, oldest first; a day with nothing recorded is absent.
#[derive(Debug, Default, PartialEq, Eq)]
struct History {
    /// Across every chain.
    days: Vec<AllChainsDay>,
    /// Per chain, by chain id.
    chains: BTreeMap<i64, Vec<DayActivity>>,
}

/// One computation's answer as served: the totals serialized once, and the
/// series the pages are cut from.
#[derive(Clone)]
pub(crate) struct Served {
    computed_at: i64,
    body: Bytes,
    history: Arc<History>,
}

impl Served {
    /// The days before the page's cursor, newest first, at most its limit.
    fn page<D: Dated + Clone>(&self, days: &[D], page: Page<Day>) -> StatsDays<D> {
        let older = match page.before {
            Some(Day(before)) => &days[..days.partition_point(|day| day.day() < before)],
            None => days,
        };
        let fetch = usize::try_from(page.fetch()).unwrap_or(usize::MAX);
        let rows = older.iter().rev().take(fetch).cloned().collect();
        let Rows { rows, next } = page.split(rows);
        StatsDays {
            computed_at: self.computed_at,
            chain_id: page.chain,
            days: rows,
            next: next.map(|day| day.to_string()),
        }
    }
}

/// An entry of a daily series.
trait Dated {
    /// Unix seconds of the day's start.
    fn day(&self) -> i64;
}

impl Dated for AllChainsDay {
    fn day(&self) -> i64 {
        self.day
    }
}

impl Dated for DayActivity {
    fn day(&self) -> i64 {
        self.day
    }
}

/// Where a page of a daily series ends: the start of the last day it
/// served, in unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Day(i64);

impl<D: Dated> Cursor<D> for Day {
    fn after(row: &D) -> Option<Self> {
        Some(Self(row.day()))
    }
}

impl FromStr for Day {
    type Err = InvalidCursor;

    fn from_str(raw: &str) -> Result<Self, InvalidCursor> {
        raw.parse().map(Self).map_err(|_| InvalidCursor)
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One computation's answer as served, and when it finished.
struct Computed {
    at: Instant,
    served: Served,
}

impl Computed {
    /// `report` as served from now on.
    fn of(report: Report) -> Result<Self, ApiError> {
        let body = serde_json::to_vec(&report.stats).map_err(ApiError::internal)?;
        Ok(Self {
            at: Instant::now(),
            served: Served {
                computed_at: report.stats.computed_at,
                body: body.into(),
                history: Arc::new(report.history),
            },
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
    ) -> Result<Served, ApiError>
    where
        F: Future<Output = Result<Report, ApiError>> + Send + 'static,
    {
        let mut latest = Arc::clone(&self.latest).lock_owned().await;
        let held = latest
            .as_ref()
            .filter(|computed| computed.fresh(Instant::now()))
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
    /// The answer from the store's aggregates: every chain the store holds,
    /// each with its totals and its days.
    fn of(snapshot: StatsSnapshot) -> Result<Self, ApiError> {
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
                .add(&row.flow)?;
        }
        for row in snapshot.onboarding_claims {
            chains
                .chain(row.chain_id)
                .span(row.day)
                .onboarding_claims
                .add(&row)?;
        }
        let mut history = History {
            days: days.into_values().collect(),
            chains: BTreeMap::new(),
        };
        let chains = chains
            .0
            .into_values()
            .map(|tally| {
                history
                    .chains
                    .insert(tally.chain_id, tally.days.into_values().collect());
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

impl TokenFlows {
    /// Count `row`: under its token, or under `other` when it names none.
    fn add(&mut self, row: &TokenFlowRow) -> Result<(), ApiError> {
        match row.token {
            Some(token) => self.tokens.push(TokenFlow::of(token, row)?),
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
    fn of(token: Address, row: &TokenFlowRow) -> Result<Self, ApiError> {
        let amount = row.amount().ok_or_else(|| {
            ApiError::internal(format_args!(
                "{token} flows on chain {}: amount {:?}",
                row.chain_id, row.amount
            ))
        })?;
        Ok(Self {
            token,
            count: row.count,
            amount,
            wallets: row.wallets,
        })
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
            total: AllChains::default(),
            chains: Vec::new(),
        }
    }

    fn answer(computed_at: i64) -> Report {
        Report {
            stats: stats(computed_at),
            history: History::default(),
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

    fn across(day: i64) -> AllChainsDay {
        AllChainsDay {
            day,
            users: 1,
            new_users: 1,
        }
    }

    /// Three days across every chain, the snapshot taken at second 7.
    fn three_days() -> Served {
        Served {
            computed_at: 7,
            body: Bytes::new(),
            history: Arc::new(History {
                days: vec![across(86_400), across(2 * 86_400), across(3 * 86_400)],
                chains: BTreeMap::new(),
            }),
        }
    }

    fn page(before: Option<i64>, limit: i64) -> Page<Day> {
        Page {
            chain: None,
            before: before.map(Day),
            limit,
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
            .get(|| async move { Report::of(waiting.stats(&[], budget).await?) })
            .await;
        let waited = started.elapsed();
        assert!(failed.is_err(), "the computation outlasted its deadline");
        assert!(
            waited >= budget && waited < 2 * budget,
            "stopped after {waited:?}"
        );
        holder.rollback().await.expect("the journal free");
        let served = cache
            .get(|| async move { Report::of(store.stats(&[], budget).await?) })
            .await;
        assert!(served.is_ok(), "the next request computes again");
    }

    /// An answer is served for five minutes after its computation.
    #[test]
    fn an_answer_is_served_for_five_minutes() {
        let at = Instant::now();
        let computed = Computed {
            at,
            served: three_days(),
        };
        assert!(computed.fresh(at));
        assert!(computed.fresh(at + STATS_TTL - Duration::from_secs(1)));
        assert!(!computed.fresh(at + STATS_TTL));
    }

    /// A page holds the newest days first, up to its limit, and hands out
    /// the cursor of its last day while older ones remain.
    #[test]
    fn a_page_of_days_runs_newest_first() {
        let served = three_days();
        let days = &served.history.days;
        let first = served.page(days, page(None, 2));
        assert_eq!(first.computed_at, 7);
        assert_eq!(first.days, [across(3 * 86_400), across(2 * 86_400)]);
        assert_eq!(first.next.as_deref(), Some("172800"));
        let before: Day = "172800".parse().expect("the cursor");
        let last = served.page(days, page(Some(before.0), 2));
        assert_eq!(last.days, [across(86_400)]);
        assert_eq!(last.next, None);
    }

    /// `before` is exclusive and need not be a day the series holds; a page
    /// of exactly the days left hands out no cursor.
    #[test]
    fn before_reads_the_days_older_than_it() {
        let served = three_days();
        let days = &served.history.days;
        let mid_day = served.page(days, page(Some(2 * 86_400 + 1), 1));
        assert_eq!(mid_day.days, [across(2 * 86_400)]);
        assert_eq!(mid_day.next.as_deref(), Some("172800"));
        let exact = served.page(days, page(None, 3));
        assert_eq!(exact.days.len(), 3);
        assert_eq!(exact.next, None);
        let none = served.page(days, page(Some(86_400), 3));
        assert!(none.days.is_empty() && none.next.is_none());
    }
}
