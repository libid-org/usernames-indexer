//! The totals reads, planned.

use alloy::primitives::Address;
use usernames_core::{
    chain::NATIVE_COIN,
    db::{
        sql,
        ListedAddress,
    },
};

use sqlx::PgConnection;

use crate::{
    explain,
    Plan,
};

/// An address's totals, planned.
pub(crate) trait TotalsPlans {
    /// The totals of an address holding nothing, of one holding many
    /// handles, and of one whose node holds thousands of spam tokens anyone can deposit and
    /// referral shares.
    async fn totals_plans(&mut self) -> Vec<Plan>;
}

impl TotalsPlans for PgConnection {
    async fn totals_plans(&mut self) -> Vec<Plan> {
        // The volume's spam: 0x7a..7a holds one node, 5000 tokens deep,
        // that 10000 binds referred.
        let holders = [
            ("totals, nothing held", Address::repeat_byte(2)),
            ("totals, many handles", Address::repeat_byte(0x4E)),
            ("totals, a node deep in spam", Address::repeat_byte(0x7A)),
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
