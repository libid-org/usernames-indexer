//! What `/v1/stats` counts, from one snapshot of the store: the handles that
//! resolve now, and the journal's users, bindings, fees and escrow
//! movements, over the whole journal and day by day.

use alloy::primitives::{
    Address,
    B256,
    U512,
};

use super::{
    sql,
    Store,
};

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

/// The wallets that used the names on one chain: linked, sent through the
/// escrow, or claimed from it.
#[derive(sqlx::FromRow)]
pub struct UsersRow {
    /// The chain.
    pub chain_id: i64,
    /// The day's start in unix seconds; `None` on the chain's total.
    pub day: Option<i64>,
    /// Distinct wallets.
    pub users: i64,
    /// Those seen that day for the first time; all of them on the total.
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

/// One token's movements of one kind on one chain.
#[derive(sqlx::FromRow)]
pub struct TokenFlowRow {
    /// The chain.
    pub chain_id: i64,
    /// The day's start in unix seconds; `None` on the chain's total.
    pub day: Option<i64>,
    /// The token.
    pub token: Address,
    /// Movements.
    pub count: i64,
    /// Their sum, as Postgres prints a `numeric`.
    pub amount: String,
    /// Distinct wallets that made them.
    pub wallets: i64,
}

impl TokenFlowRow {
    /// The sum as a number, under [`FeesRow::amount`]'s rule.
    pub fn amount(&self) -> Option<U512> {
        self.amount.parse().ok()
    }
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

impl Store {
    /// Every number `/v1/stats` serves, read in one snapshot so they agree
    /// with each other. The aggregates read whole tables, so a caller runs
    /// them once per period, not per request.
    pub async fn stats(&self) -> Result<StatsSnapshot, sqlx::Error> {
        let mut tx = self.pool.begin_with(sql::STATS_SNAPSHOT).await?;
        let snapshot = StatsSnapshot {
            taken_at: sqlx::query_scalar(sql::STATS_TAKEN_AT)
                .fetch_one(&mut *tx)
                .await?,
            chains: sqlx::query_scalar(sql::CHAINS).fetch_all(&mut *tx).await?,
            handles: sqlx::query_as(sql::STATS_HANDLES)
                .fetch_all(&mut *tx)
                .await?,
            users: sqlx::query_as(sql::STATS_USERS).fetch_all(&mut *tx).await?,
            links: sqlx::query_as(sql::STATS_LINKS).fetch_all(&mut *tx).await?,
            fees: sqlx::query_as(sql::STATS_FEES).fetch_all(&mut *tx).await?,
            escrow: sqlx::query_as(sql::STATS_ESCROW)
                .fetch_all(&mut *tx)
                .await?,
            onboarding_claims: sqlx::query_as(sql::STATS_ONBOARDING)
                .fetch_all(&mut *tx)
                .await?,
        };
        tx.commit().await?;
        Ok(snapshot)
    }
}
