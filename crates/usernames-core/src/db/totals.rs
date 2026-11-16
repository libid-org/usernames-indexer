//! An address's unclaimed totals per chain and token: what it may claim
//! now and what binding its handles elsewhere makes claimable, summed over
//! the named tokens only, since anybody can deposit a token they wrote.

use alloy::primitives::{
    Address,
    U512,
};

use super::{
    sql,
    ListedAddress,
    Store,
};
use crate::chain::NATIVE_COIN;

/// One chain and token's unclaimed totals for an address, in token units as
/// Postgres prints a `numeric` sum.
#[derive(Debug, sqlx::FromRow)]
pub struct TotalsRow {
    /// The chain the escrow holds them on.
    pub chain_id: i64,
    /// The token; EIP-7528 `0xEeee…EEeE` is the chain's own coin.
    pub token: Address,
    /// Held for the nodes the address holds on this chain.
    pub claimable: String,
    /// Held for its nodes on this chain, where nobody holds them.
    pub awaiting: String,
    /// Of `claimable`, the referral shares held for its nodes.
    pub referred_claimable: String,
    /// Of `awaiting`, the referral shares held for its nodes.
    pub referred_awaiting: String,
}

impl TotalsRow {
    /// The two sums as numbers: `None` only for a stored value that is not
    /// a sum of `uint256`s, which the books never write.
    pub fn amounts(&self) -> Option<(U512, U512)> {
        Some((self.claimable.parse().ok()?, self.awaiting.parse().ok()?))
    }

    /// The referral shares inside the two sums, as numbers, like
    /// [`Self::amounts`].
    pub fn referred(&self) -> Option<(U512, U512)> {
        Some((
            self.referred_claimable.parse().ok()?,
            self.referred_awaiting.parse().ok()?,
        ))
    }
}

impl Store {
    /// `holder`'s unclaimed totals in each chain's coin and in `tokens`, by
    /// chain and token: a row where anything is held.
    pub async fn escrow_totals(
        &self,
        holder: Address,
        tokens: &[ListedAddress],
    ) -> Result<Vec<TotalsRow>, sqlx::Error> {
        let (chains, addresses) = ListedAddress::columns(tokens);
        sqlx::query_as(sql::ESCROW_TOTALS)
            .bind(holder)
            .bind(chains)
            .bind(addresses)
            .bind(NATIVE_COIN.to_vec())
            .fetch_all(&self.pool)
            .await
    }
}
