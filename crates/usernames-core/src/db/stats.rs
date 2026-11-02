//! What `/v1/stats` counts, from one snapshot of the store: the handles that
//! resolve now, and the journal's users, bindings, fees and escrow
//! movements, over the whole journal and day by day.

use std::{
    fmt,
    num::ParseIntError,
    str::FromStr,
};

use alloy::primitives::{
    hex::FromHexError,
    Address,
    B256,
    U512,
};

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

/// One token's movements of one kind on one chain, or every unlisted
/// token's together.
#[derive(sqlx::FromRow)]
pub struct TokenFlowRow {
    /// The chain.
    pub chain_id: i64,
    /// The day's start in unix seconds; `None` on the chain's total.
    pub day: Option<i64>,
    /// The token: the chain's coin or a listed one; `None` for every other
    /// token together.
    pub token: Option<Address>,
    /// Movements.
    pub count: i64,
    /// Their sum, as Postgres prints a `numeric`; `None` for the unlisted
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

/// A token `/v1/stats` names on one chain, beside the chain's own coin,
/// written `<chain id>:<address>`. Every other token counts under `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedToken {
    /// The chain.
    pub chain_id: i64,
    /// The token's address there.
    pub token: Address,
}

/// Why a listed token does not parse.
#[derive(Debug, thiserror::Error)]
pub enum ListedTokenError {
    /// No `:` between the chain id and the address.
    #[error("{0:?} is not <chain id>:<address>")]
    Shape(String),
    /// The chain id is not a number.
    #[error("chain id {0:?}: {1}")]
    Chain(String, ParseIntError),
    /// The address is not one.
    #[error("token {0:?}: {1}")]
    Token(String, FromHexError),
}

impl FromStr for ListedToken {
    type Err = ListedTokenError;

    fn from_str(listed: &str) -> Result<Self, ListedTokenError> {
        let (chain, token) = listed
            .split_once(':')
            .ok_or_else(|| ListedTokenError::Shape(listed.to_string()))?;
        Ok(Self {
            chain_id: chain
                .parse()
                .map_err(|e| ListedTokenError::Chain(chain.to_string(), e))?,
            token: token
                .parse()
                .map_err(|e| ListedTokenError::Token(token.to_string(), e))?,
        })
    }
}

impl fmt::Display for ListedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.chain_id, self.token)
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
    /// with each other. Token flows name each chain's coin and the `listed`
    /// tokens, and fold every other token into one row. The aggregates read
    /// whole tables, so a caller runs them once per period, not per request.
    pub async fn stats(
        &self,
        listed: &[ListedToken],
    ) -> Result<StatsSnapshot, sqlx::Error> {
        let chains: Vec<i64> = listed.iter().map(|listed| listed.chain_id).collect();
        let tokens: Vec<Vec<u8>> =
            listed.iter().map(|listed| listed.token.to_vec()).collect();
        let native = NATIVE_COIN.to_vec();
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
                .bind(&native)
                .bind(&chains)
                .bind(&tokens)
                .fetch_all(&mut *tx)
                .await?,
            onboarding_claims: sqlx::query_as(sql::STATS_ONBOARDING)
                .bind(&native)
                .bind(&chains)
                .bind(&tokens)
                .fetch_all(&mut *tx)
                .await?,
        };
        tx.commit().await?;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listed token reads from its `<chain id>:<address>` text and prints
    /// back to it; anything else is refused.
    #[test]
    fn a_listed_token_parses_from_its_text() {
        let text = "8453:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
        let listed: ListedToken = text.parse().expect("a listed token");
        assert_eq!(listed.chain_id, 8453);
        assert_eq!(listed.to_string(), text);
        for refused in [
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "base:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "8453:0x8335",
        ] {
            assert!(refused.parse::<ListedToken>().is_err(), "{refused}");
        }
    }
}
