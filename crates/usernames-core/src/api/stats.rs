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
        AllChainsFeeIncome,
        Amount,
        ChainStats,
        DayActivity,
        FeeIncome,
        FeeSum,
        FeesPaid,
        OtherFees,
        OtherTokens,
        PlatformHandles,
        PlatformLinks,
        RegistrationFees,
        Stats,
        StatsDays,
        TokenFlow,
        TokenFlows,
        TokenValue,
        UsdTotal,
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
    chain::NATIVE_COIN,
    db::{
        CountRow,
        FeeReceivers,
        Flow,
        HandlesRow,
        HeldSharesRow,
        ListedAddress,
        Listing,
        Measure,
        NamedTokens,
        Share,
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
    /// The prices the served totals hold, to value amounts on a request
    /// path without asking the source: none, with a warning, when the
    /// totals cannot be computed, so an amount list never fails on them.
    pub(crate) async fn quotes(&self) -> Arc<Quotes> {
        match self.clone().served_stats().await {
            Ok(served) => served.quotes,
            Err(error) => {
                tracing::warn!(%error, "amounts served unpriced: the stats computation failed");
                Arc::default()
            }
        }
    }

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
/// for `ttl`; a computation may take `deadline`, and one that failed is not
/// run again for as long.
#[derive(Clone)]
pub struct StatsCache {
    latest: Arc<Mutex<Latest>>,
    ttl: Duration,
    deadline: Duration,
}

/// What the cache holds: the last answer computed, and when the last
/// computation failed, if it was the latest.
#[derive(Default)]
struct Latest {
    computed: Option<Computed>,
    failed: Option<Instant>,
}

impl Default for StatsCache {
    fn default() -> Self {
        Self::new(STATS_TTL, STATS_DEADLINE)
    }
}

/// One computation's totals and the prices that valued them.
pub struct Valued {
    /// The totals.
    pub stats: Stats,
    /// The prices that valued them.
    pub quotes: Quotes,
}

/// The numbers a snapshot holds, as the stats routes serve them: the totals,
/// and the days, oldest first.
pub struct Report {
    /// The totals.
    pub stats: Stats,
    /// The days, oldest first.
    pub history: History,
}

/// The days of a snapshot, oldest first; a day with nothing recorded is
/// absent.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Across every chain.
    days: Vec<AllChainsDay>,
    /// Per chain, by chain id.
    chains: BTreeMap<i64, Vec<DayActivity>>,
}

/// The totals as served: serialized once, with the prices that valued
/// them, which name and value the tokens of a page of days too.
#[derive(Clone)]
pub struct Served {
    /// The totals, serialized.
    pub body: Bytes,
    /// The prices that valued them.
    pub quotes: Arc<Quotes>,
}

/// One computation's answer as served, and when it finished.
pub struct Computed {
    /// When the computation finished.
    pub at: Instant,
    /// Its answer.
    pub served: Served,
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
    pub fn fresh(&self, now: Instant, ttl: Duration) -> bool {
        now.saturating_duration_since(self.at) < ttl
    }
}

impl StatsCache {
    /// An empty cache serving each answer for `ttl`, each computation
    /// given `deadline`.
    pub fn new(ttl: Duration, deadline: Duration) -> Self {
        Self {
            latest: Arc::default(),
            ttl,
            deadline,
        }
    }

    /// The answer, from `compute` when none younger than the cache's `ttl`
    /// is held. The lock is held through the computation, so requests arriving
    /// meanwhile wait for its answer instead of computing again. A failure is
    /// answered at once for `deadline` after it, so a burst against a failing
    /// computation waits on none; the first request after that computes. The
    /// computation runs in a task of its own: a request whose client goes
    /// away leaves it running.
    pub async fn get<F>(&self, compute: impl FnOnce() -> F) -> Result<Served, ApiError>
    where
        F: Future<Output = Result<Valued, ApiError>> + Send + 'static,
    {
        let mut latest = Arc::clone(&self.latest).lock_owned().await;
        let now = Instant::now();
        let held = latest
            .computed
            .as_ref()
            .filter(|computed| computed.fresh(now, self.ttl))
            .map(|computed| computed.served.clone());
        if let Some(served) = held {
            drop(latest);
            return Ok(served);
        }
        if let Some(failed) = latest.failed {
            if now.saturating_duration_since(failed) < self.deadline {
                drop(latest);
                return Err(ApiError::stats_backing_off());
            }
        }
        let computation = compute();
        tokio::spawn(async move {
            let answer = match computation.await.and_then(Computed::of) {
                Ok(computed) => {
                    latest.failed = None;
                    Ok(latest.computed.insert(computed).served.clone())
                }
                Err(error) => {
                    latest.failed = Some(Instant::now());
                    Err(error)
                }
            };
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
    pub fn of(snapshot: StatsSnapshot, quotes: &Quotes) -> Result<Self, ApiError> {
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
        let receivers = &snapshot.fee_receivers;
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
                (Measure::Fees, day) => chain.span(day).fee(row, receivers)?,
                (Measure::Flow(flow), day) => {
                    chain.span(day).flows(flow).add(row, quotes)?
                }
                (Measure::Shared(share), day) => chain
                    .span(day)
                    .fee_income
                    .share(share)
                    .add(row.count, row.amount_of()?),
            }
        }
        for row in &snapshot.held_shares {
            let held = row.amount_of()?;
            chains
                .chain(row.chain_id)
                .awaiting_claim
                .add(row.count, held);
        }
        let mut history = History {
            days: days.into_values().collect(),
            chains: BTreeMap::new(),
        };
        let chains = chains
            .0
            .into_values()
            .map(|mut tally| {
                let coin = quotes.of(tally.chain_id, NATIVE_COIN);
                tally.total.settle(coin);
                tally.awaiting_claim.price(coin);
                total.payments.add(&tally.total.escrow.deposits);
                total.payments.add(&tally.total.escrow.forwards);
                total.fee_income.add(&tally.total.fee_income);
                let days = tally.days.into_values().map(|mut day| {
                    day.activity.settle(coin);
                    day
                });
                history.chains.insert(tally.chain_id, days.collect());
                ChainStats {
                    chain_id: tally.chain_id,
                    handles: tally.handles,
                    total: tally.total,
                    awaiting_claim: tally.awaiting_claim,
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
    awaiting_claim: FeeSum,
    days: BTreeMap<i64, DayActivity>,
}

impl Tally {
    fn new(chain_id: i64) -> Self {
        Self {
            chain_id,
            handles: Vec::new(),
            total: Activity::default(),
            awaiting_claim: FeeSum::default(),
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

    /// Count a fee row: under its receiver when listed, among the fees
    /// collected when its receiver is listed or the chain's referral
    /// splitter, and as every receiver's fees when it names none.
    fn fee(&mut self, row: &CountRow, receivers: &FeeReceivers) -> Result<(), ApiError> {
        if row.key.is_empty() {
            return self.registration_fees.every(row);
        }
        let receiver = ListedAddress {
            chain_id: row.chain_id,
            address: row.key_as()?,
        };
        if receivers.lists(&receiver) {
            self.registration_fees.receivers.push(FeesPaid::of(row)?);
        }
        if receivers.collects(&receiver) {
            self.fee_income.collected.add(row.count, row.amount_of()?);
        }
        Ok(())
    }

    /// Once every row is in: `other` only where the tokens or receivers
    /// nobody named moved or were paid, and the fee income valued at the
    /// chain's coin, `coin`.
    fn settle(&mut self, coin: Option<&Quote>) {
        for flow in Flow::ALL {
            let flows = self.flows(flow);
            flows.other = flows.other.take().filter(|other| other.count > 0);
        }
        self.registration_fees.settle();
        self.fee_income.price(coin);
    }
}

impl FeeIncome {
    /// The shares a row of `share` counts in.
    fn share(&mut self, share: Share) -> &mut FeeSum {
        match share {
            Share::Forwarded => &mut self.forwarded,
            Share::Escrowed => &mut self.escrowed,
        }
    }

    /// Value every sum at the chain's coin, `coin`, once every row is in.
    fn price(&mut self, coin: Option<&Quote>) {
        for sum in [&mut self.collected, &mut self.forwarded, &mut self.escrowed] {
            sum.price(coin);
        }
    }
}

impl FeeSum {
    /// Count `count` more fees or shares, summing `amount`.
    fn add(&mut self, count: i64, amount: U512) {
        self.count += count;
        self.amount.amount = self.amount.amount.saturating_add(amount);
    }

    /// Value the sum at the chain's coin, `coin`, once every row is in.
    fn price(&mut self, coin: Option<&Quote>) {
        let amount = self.amount.amount;
        self.amount = Amount::of(amount, TokenValue::of(amount, coin));
    }
}

impl AllChainsFeeIncome {
    /// Count one chain's fee income, once its sums are valued.
    fn add(&mut self, chain: &FeeIncome) {
        self.collected.count(&chain.collected);
        self.shared.count(&chain.forwarded);
        self.shared.count(&chain.escrowed);
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

impl HeldSharesRow {
    /// The amount, or the error naming the row's chain.
    fn amount_of(&self) -> Result<U512, ApiError> {
        self.amount().ok_or_else(|| {
            ApiError::internal(format_args!(
                "held shares on chain {}: amount {:?}",
                self.chain_id, self.amount
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
    /// Count `row`, every receiver's fees, which `other` holds until
    /// `settle`.
    fn every(&mut self, row: &CountRow) -> Result<(), ApiError> {
        self.other = Some(OtherFees {
            count: row.count,
            amount: row.amount_of()?,
        });
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

impl UsdTotal {
    /// Count the payments of `flows`, once every row is in.
    fn add(&mut self, flows: &TokenFlows) {
        for flow in &flows.tokens {
            match flow.priced.usd.and_then(|usd| self.usd.checked_add(usd)) {
                Some(usd) => self.usd = usd,
                None => self.unpriced += flow.count,
            }
        }
        self.unpriced += flows.other.as_ref().map_or(0, |other| other.count);
    }

    /// Count `sum`, once valued: its USD, or its count when unpriced.
    fn count(&mut self, sum: &FeeSum) {
        match sum.amount.usd.and_then(|usd| self.usd.checked_add(usd)) {
            Some(usd) => self.usd = usd,
            None => self.unpriced += sum.count,
        }
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
                let mut flow = TokenFlow::of(token, row, quotes.of(row.chain_id, token))?;
                match flow.priced.usd.map(|usd| self.usd.checked_add(usd)) {
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
        Ok(Self {
            token,
            count: row.count,
            amount,
            wallets: row.wallets,
            priced: TokenValue::of(amount, quote),
        })
    }

    /// The flow without a price, as when its sum would overflow.
    fn unprice(&mut self) {
        self.priced = TokenValue::default();
    }
}
