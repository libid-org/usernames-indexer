//! The stats cache: one computation served, failures backed off.

use axum::body::Bytes;
use std::{
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};
use usernames_core::{
    api::{
        error::ApiError,
        model::{
            AllChains,
            Stats,
        },
        prices::Quotes,
    },
    db::Listing,
};

use std::sync::atomic::{
    AtomicI64,
    Ordering,
};

use futures_util::future::join_all;
use sqlx::PgPool;
use tokio::sync::oneshot;
use usernames_fixtures::database_url;

use usernames_core::{
    api::stats::*,
    db::{
        Store,
        MIGRATOR,
    },
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

/// A failure is answered at once for the deadline after it: a request
/// meanwhile runs no computation of its own.
#[tokio::test]
async fn a_failure_is_answered_without_computing_for_its_deadline() {
    let cache = StatsCache::default();
    let computations = Arc::new(AtomicI64::new(0));
    let failing = || {
        let computations = Arc::clone(&computations);
        async move {
            computations.fetch_add(1, Ordering::SeqCst);
            Err(ApiError::internal("the database is down"))
        }
    };
    assert!(cache.get(failing).await.is_err());
    let next = cache.get(|| async { Ok(answer(1)) }).await;
    assert!(next.is_err(), "answered the failure");
    assert_eq!(computations.load(Ordering::SeqCst), 1);
}

/// A failed computation is not served: past its deadline, here none,
/// the next request computes.
#[tokio::test]
async fn a_failed_computation_is_computed_again() {
    let cache = StatsCache::new(STATS_TTL, Duration::ZERO);
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
    let url = database_url();
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
    // No backoff after the failure, so the next request computes.
    let cache = StatsCache::new(STATS_TTL, Duration::ZERO);
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
