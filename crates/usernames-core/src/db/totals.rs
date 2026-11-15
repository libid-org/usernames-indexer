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

#[cfg(test)]
pub(crate) mod plans {
    use sqlx::PgConnection;

    use super::*;
    use crate::db::plan_tests::{
        explain,
        Plan,
    };

    /// An address's totals, planned.
    pub(crate) trait TotalsPlans {
        /// The totals of an address holding nothing, of one holding many
        /// handles, and of one whose node holds thousands of junk tokens and
        /// referral shares.
        async fn totals_plans(&mut self) -> Vec<Plan>;
    }

    impl TotalsPlans for PgConnection {
        async fn totals_plans(&mut self) -> Vec<Plan> {
            // The volume's junk: 0x7a..7a holds one node, 5000 tokens deep,
            // that 10000 binds referred.
            let holders = [
                ("totals, nothing held", Address::repeat_byte(2)),
                ("totals, many handles", Address::repeat_byte(0x4E)),
                ("totals, a node deep in junk", Address::repeat_byte(0x7A)),
            ];
            // The coin is named on every chain; one priced token on chain 1.
            let (chains, tokens) = ListedAddress::columns(&[ListedAddress {
                chain_id: 1,
                address: Address::repeat_byte(0xE5),
            }]);
            let seeks: &[&str] = &[
                "handles_owner_chain_node_idx",
                "escrow_held_pkey|escrow_held_node_idx:chain_id,handle_node,token",
                "referrals_held_idx:chain_id,referrer_node,token",
            ];
            let mut plans = Vec::new();
            for (name, holder) in holders {
                let text = sqlx::query_scalar::<_, String>(&explain(sql::ESCROW_TOTALS))
                    .bind(holder)
                    .bind(&chains)
                    .bind(&tokens)
                    .bind(NATIVE_COIN.to_vec())
                    .fetch_all(&mut *self)
                    .await
                    .unwrap_or_else(|e| panic!("{name}: {e}"))
                    .join("\n");
                plans.push(
                    Plan {
                        name,
                        seeks: Some(seeks),
                        text,
                    }
                    .reading_once(holder),
                );
            }
            plans
        }
    }
}
