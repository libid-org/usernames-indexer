//! What `/v1/stats` counts, from one snapshot of the store: the handles that
//! resolve now, and the journal's users, bindings, fees and escrow
//! movements, over the whole journal and day by day.

use std::{
    future::Future,
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
use sqlx::PgConnection;

use super::{
    sql,
    Store,
};
use crate::chain::NATIVE_COIN;

/// Every number `/v1/stats` serves, as the store aggregates them. Each
/// journal list holds a chain's totals, the rows with no day, and one row
/// per UTC day with something in it.
pub struct StatsSnapshot {
    /// When the snapshot was taken, in unix seconds by the database's clock.
    pub taken_at: i64,
    /// Every chain an indexer prepared, by id.
    pub chains: Vec<i64>,
    /// The handles that resolve now, per chain and platform.
    pub handles: Vec<HandlesRow>,
    /// The wallets that used the names, per chain.
    pub users: Vec<UsersRow>,
    /// Bindings, per chain and platform.
    pub links: Vec<LinksRow>,
    /// Registration fees, per chain and receiver.
    pub fees: Vec<FeesRow>,
    /// Escrow movements, per chain, kind and token.
    pub escrow: Vec<EscrowFlowRow>,
    /// Claims that brought their holder in, per chain and token.
    pub onboarding_claims: Vec<TokenFlowRow>,
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

/// The wallets that used the names on one chain, or across every chain:
/// linked, sent through the escrow, or claimed from it.
#[derive(sqlx::FromRow)]
pub struct UsersRow {
    /// The chain; `None` on the rows across every chain.
    pub chain_id: Option<i64>,
    /// The day's start in unix seconds; `None` on the total.
    pub day: Option<i64>,
    /// Distinct wallets.
    pub users: i64,
    /// Those seen that day for the first time, on the row's chain or, across
    /// every chain, on any of them; all of them on the total.
    pub new_users: i64,
}

/// The bindings made on one platform on one chain.
#[derive(sqlx::FromRow)]
pub struct LinksRow {
    /// The chain.
    pub chain_id: i64,
    /// The day's start in unix seconds; `None` on the chain's total.
    pub day: Option<i64>,
    /// The platform.
    pub platform_id: B256,
    /// Bindings made.
    pub count: i64,
    /// Distinct wallets they bound to.
    pub holders: i64,
}

/// The registration fees one receiver was paid on one chain.
#[derive(sqlx::FromRow)]
pub struct FeesRow {
    /// The chain.
    pub chain_id: i64,
    /// The day's start in unix seconds; `None` on the chain's total.
    pub day: Option<i64>,
    /// The receiver.
    pub receiver: Address,
    /// Fees paid.
    pub count: i64,
    /// Their sum, as Postgres prints a `numeric`.
    pub amount: String,
}

impl FeesRow {
    /// The sum as a number. `None` only for text that is not an unsigned
    /// integer below 2^512, which a sum of the journal's `uint256` amounts
    /// never is.
    pub fn amount(&self) -> Option<U512> {
        self.amount.parse().ok()
    }
}

/// One token's movements of one kind on one chain, or every unnamed
/// token's together.
#[derive(sqlx::FromRow)]
pub struct TokenFlowRow {
    /// The chain.
    pub chain_id: i64,
    /// The day's start in unix seconds; `None` on the chain's total.
    pub day: Option<i64>,
    /// The token: the chain's coin or a named one; `None` for every other
    /// token together.
    pub token: Option<Address>,
    /// Movements.
    pub count: i64,
    /// Their sum, as Postgres prints a `numeric`; `None` for the unnamed
    /// tokens, whose units do not add up.
    pub amount: Option<String>,
    /// Distinct wallets that made them.
    pub wallets: i64,
}

impl TokenFlowRow {
    /// The sum as a number, under [`FeesRow::amount`]'s rule; `None` too
    /// when there is no sum.
    pub fn amount(&self) -> Option<U512> {
        self.amount.as_deref()?.parse().ok()
    }
}

/// A token on one chain: a coin `/v1/stats` asks a price for, and names
/// once priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, sqlx::FromRow)]
pub struct ChainToken {
    /// The chain.
    pub chain_id: i64,
    /// The token's address there.
    pub token: Address,
}

impl ChainToken {
    /// `tokens` as the two arrays a statement unnests into pairs.
    fn columns(tokens: &[Self]) -> (Vec<i64>, Vec<Vec<u8>>) {
        tokens
            .iter()
            .map(|token| (token.chain_id, token.token.to_vec()))
            .unzip()
    }
}

/// The tokens a computation names, once it has their prices.
pub trait NamedTokens {
    /// The tokens named beside each chain's coin.
    fn named(&self) -> Vec<ChainToken>;
}

/// One token's escrow movements of one kind.
#[derive(sqlx::FromRow)]
pub struct EscrowFlowRow {
    /// The journal's name of the event kind.
    pub kind: String,
    /// The movements.
    #[sqlx(flatten)]
    pub flow: TokenFlowRow,
}

/// The tokens per chain asked a price for beyond its coin and those priced
/// before. DefiLlama answers 100 coins a request; five chains take two.
pub const CANDIDATES_PER_CHAIN: i64 = 40;

impl Store {
    /// Every number `/v1/stats` serves, read in one snapshot so they agree
    /// with each other. The snapshot first reads the tokens worth a price:
    /// each chain's coin, the `kept` tokens and the most moved others, up
    /// to [`CANDIDATES_PER_CHAIN`]. `price` values them while the snapshot
    /// reads the aggregates; the token flows then name each chain's coin and
    /// the tokens it named, and fold every other token into one row. The
    /// aggregates read whole tables, so a caller runs them once per period,
    /// not per request. The statements share `budget`: the database stops
    /// the one running when it is spent.
    pub async fn stats<F, P>(
        &self,
        kept: &[ChainToken],
        budget: Duration,
        price: impl FnOnce(Vec<ChainToken>) -> F,
    ) -> Result<(StatsSnapshot, P), sqlx::Error>
    where
        F: Future<Output = P>,
        P: NamedTokens,
    {
        let deadline = Deadline::after(budget);
        let native = NATIVE_COIN.to_vec();
        let (kept_chains, kept_tokens) = ChainToken::columns(kept);
        let mut tx = self.pool.begin_with(sql::STATS_SNAPSHOT).await?;
        let candidates = sqlx::query_as(sql::STATS_CANDIDATES)
            .bind(&native)
            .bind(&kept_chains)
            .bind(&kept_tokens)
            .bind(CANDIDATES_PER_CHAIN)
            .fetch_all(deadline.bound(&mut tx).await?)
            .await?;
        let (prices, aggregates) =
            tokio::join!(price(candidates), Aggregates::read(&mut tx, &deadline));
        let aggregates = aggregates?;
        let (chains, tokens) = ChainToken::columns(&prices.named());
        let snapshot = StatsSnapshot {
            taken_at: aggregates.taken_at,
            chains: aggregates.chains,
            handles: aggregates.handles,
            users: aggregates.users,
            links: aggregates.links,
            fees: aggregates.fees,
            escrow: sqlx::query_as(sql::STATS_ESCROW)
                .bind(&native)
                .bind(&chains)
                .bind(&tokens)
                .fetch_all(deadline.bound(&mut tx).await?)
                .await?,
            onboarding_claims: sqlx::query_as(sql::STATS_ONBOARDING)
                .bind(&native)
                .bind(&chains)
                .bind(&tokens)
                .fetch_all(deadline.bound(&mut tx).await?)
                .await?,
        };
        tx.commit().await?;
        Ok((snapshot, prices))
    }
}

/// The numbers of a snapshot that need no prices.
struct Aggregates {
    taken_at: i64,
    chains: Vec<i64>,
    handles: Vec<HandlesRow>,
    users: Vec<UsersRow>,
    links: Vec<LinksRow>,
    fees: Vec<FeesRow>,
}

impl Aggregates {
    /// The aggregates, read on `conn` within `deadline`.
    async fn read(
        conn: &mut PgConnection,
        deadline: &Deadline,
    ) -> Result<Self, sqlx::Error> {
        Ok(Self {
            taken_at: sqlx::query_scalar(sql::STATS_TAKEN_AT)
                .fetch_one(deadline.bound(conn).await?)
                .await?,
            chains: sqlx::query_scalar(sql::CHAINS)
                .fetch_all(deadline.bound(conn).await?)
                .await?,
            handles: sqlx::query_as(sql::STATS_HANDLES)
                .fetch_all(deadline.bound(conn).await?)
                .await?,
            users: sqlx::query_as(sql::STATS_USERS)
                .fetch_all(deadline.bound(conn).await?)
                .await?,
            links: sqlx::query_as(sql::STATS_LINKS)
                .fetch_all(deadline.bound(conn).await?)
                .await?,
            fees: sqlx::query_as(sql::STATS_FEES)
                .fetch_all(deadline.bound(conn).await?)
                .await?,
        })
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
    use crate::db::plan_tests::{
        explain,
        Plan,
    };

    /// The `/v1/stats` aggregates, planned.
    pub(crate) trait StatsPlans {
        /// Each aggregate, with the covering indexes it reads whole and
        /// without the heap.
        async fn stats_plans(&mut self) -> Vec<Plan>;
    }

    impl StatsPlans for PgConnection {
        async fn stats_plans(&mut self) -> Vec<Plan> {
            let (native, chains, tokens) =
                (NATIVE_COIN.to_vec(), [1i64], [vec![0xEE; 20]]);
            let reads = [
                (
                    "the live handles",
                    &["Index Only Scan using handles_live_idx"][..],
                    sqlx::query_scalar::<_, String>(&explain(sql::STATS_HANDLES))
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the users",
                    &[
                        "Index Only Scan using events_users_wallet_idx",
                        "Index Only Scan using events_users_day_idx",
                        "Index Only Scan using events_users_all_chains_day_idx",
                    ][..],
                    sqlx::query_scalar(&explain(sql::STATS_USERS))
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the links",
                    &["Index Only Scan using events_links_idx"][..],
                    sqlx::query_scalar(&explain(sql::STATS_LINKS))
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the registration fees",
                    &["Index Only Scan using events_fees_idx"][..],
                    sqlx::query_scalar(&explain(sql::STATS_FEES))
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the escrow flows",
                    &["Index Only Scan using events_escrow_idx"][..],
                    sqlx::query_scalar(&explain(sql::STATS_ESCROW))
                        .bind(&native)
                        .bind(&chains[..])
                        .bind(&tokens[..])
                        .fetch_all(&mut *self)
                        .await,
                ),
                (
                    "the onboarding claims",
                    &[
                        "Index Only Scan using events_claims_idx",
                        "Index Only Scan using events_first_deposit_idx",
                        "Index Only Scan using events_binds_idx",
                    ][..],
                    sqlx::query_scalar(&explain(sql::STATS_ONBOARDING))
                        .bind(&native)
                        .bind(&chains[..])
                        .bind(&tokens[..])
                        .fetch_all(&mut *self)
                        .await,
                ),
            ];
            let mut plans = Vec::new();
            for (name, seeks, text) in reads {
                let text: Vec<String> = text.unwrap_or_else(|e| panic!("{name}: {e}"));
                plans.push(Plan {
                    name,
                    seeks: Some(seeks),
                    text: text.join("\n"),
                });
            }
            plans
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
        let Some(url) = database_url() else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
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
}
