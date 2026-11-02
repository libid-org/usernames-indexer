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

use axum::{
    extract::State,
    Json,
};
use tokio::sync::Mutex;

use super::{
    error::ApiError,
    model::{
        Activity,
        ChainStats,
        DayActivity,
        EscrowActivity,
        FeesPaid,
        PlatformHandles,
        PlatformLinks,
        Stats,
        TokenFlow,
    },
    AppState,
};
use crate::{
    db::{
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

/// `GET /v1/stats` — totals and a weekly series per chain, from one
/// snapshot of the store.
pub(crate) async fn stats(
    State(state): State<AppState>,
) -> Result<Json<Stats>, ApiError> {
    let stats = state
        .stats
        .get(|| async { Stats::of(state.store.stats().await?) })
        .await?;
    Ok(Json(stats))
}

/// The latest answer, shared by every request.
#[derive(Clone, Default)]
pub(crate) struct StatsCache {
    latest: Arc<Mutex<Option<Computed>>>,
}

/// One computation's answer, and when it finished.
struct Computed {
    at: Instant,
    stats: Stats,
}

impl Computed {
    /// Whether the answer may still be served at `now`.
    fn fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) < STATS_TTL
    }
}

impl StatsCache {
    /// The answer, from `compute` when none younger than [`STATS_TTL`] is
    /// held. The lock is held through the computation, so requests arriving
    /// meanwhile wait for its answer instead of computing again; after a
    /// failure the next of them computes.
    pub(crate) async fn get<F>(
        &self,
        compute: impl FnOnce() -> F,
    ) -> Result<Stats, ApiError>
    where
        F: Future<Output = Result<Stats, ApiError>>,
    {
        let mut latest = self.latest.lock().await;
        let held = latest
            .as_ref()
            .filter(|computed| computed.fresh(Instant::now()))
            .map(|computed| computed.stats.clone());
        let answer = match held {
            Some(stats) => Ok(stats),
            None => compute().await.inspect(|stats| {
                *latest = Some(Computed {
                    at: Instant::now(),
                    stats: stats.clone(),
                });
            }),
        };
        drop(latest);
        answer
    }
}

impl Stats {
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
        for row in snapshot.users {
            let chain = chains.chain(row.chain_id);
            match row.day {
                Some(day) => {
                    let day = chain.day(day);
                    day.new_users = row.new_users;
                    day.activity.users = row.users;
                }
                None => chain.total.users = row.users,
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
            let flow = TokenFlow::of(&row.flow)?;
            chains
                .chain(row.flow.chain_id)
                .span(row.flow.day)
                .escrow
                .flows(kind)?
                .push(flow);
        }
        for row in snapshot.onboarding_claims {
            let flow = TokenFlow::of(&row)?;
            chains
                .chain(row.chain_id)
                .span(row.day)
                .onboarding_claims
                .push(flow);
        }
        Ok(Self {
            computed_at: snapshot.taken_at,
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
    /// The list an escrow event of `kind` counts in.
    fn flows(&mut self, kind: EventKind) -> Result<&mut Vec<TokenFlow>, ApiError> {
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

impl TokenFlow {
    fn of(row: &TokenFlowRow) -> Result<Self, ApiError> {
        let amount = row.amount().ok_or_else(|| {
            ApiError::internal(format_args!(
                "{} flows on chain {}: amount {:?}",
                row.token, row.chain_id, row.amount
            ))
        })?;
        Ok(Self {
            token: row.token,
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

    use super::*;

    fn answer(computed_at: i64) -> Stats {
        Stats {
            computed_at,
            chains: Vec::new(),
        }
    }

    /// Ten requests that arrive together run one computation, and each gets
    /// its answer.
    #[tokio::test]
    async fn requests_arriving_together_share_one_computation() {
        let cache = StatsCache::default();
        let computations = AtomicI64::new(0);
        let compute = || async {
            let computation = computations.fetch_add(1, Ordering::SeqCst);
            // The other requests are polled while this one computes.
            tokio::task::yield_now().await;
            Ok(answer(computation))
        };
        let answers = join_all((0..10).map(|_| cache.get(compute))).await;
        assert_eq!(computations.load(Ordering::SeqCst), 1);
        for served in answers {
            assert_eq!(served.ok(), Some(answer(0)));
        }
    }

    /// A failed computation is not served: the next request computes.
    #[tokio::test]
    async fn a_failed_computation_is_computed_again() {
        let cache = StatsCache::default();
        let failed = cache
            .get(|| async { Err(ApiError::internal("the database is down")) })
            .await;
        assert!(failed.is_err());
        let served = cache.get(|| async { Ok(answer(1)) }).await;
        assert_eq!(served.ok(), Some(answer(1)));
    }

    /// An answer is served for five minutes after its computation.
    #[test]
    fn an_answer_is_served_for_five_minutes() {
        let at = Instant::now();
        let computed = Computed {
            at,
            stats: answer(0),
        };
        assert!(computed.fresh(at));
        assert!(computed.fresh(at + STATS_TTL - Duration::from_secs(1)));
        assert!(!computed.fresh(at + STATS_TTL));
    }
}
