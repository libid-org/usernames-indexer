//! The links reads, planned.

use alloy::primitives::{
    Address,
    B256,
};

use sqlx::PgConnection;

use crate::{
    explain,
    Plan,
};
use usernames_core::db::links::*;

/// A history page's referral links, planned.
pub(crate) trait ReferralLinkPlans {
    /// The links of a full page: referred entries and splitter payments.
    async fn referral_link_plans(&mut self) -> Vec<Plan>;
}

impl ReferralLinkPlans for PgConnection {
    async fn referral_link_plans(&mut self) -> Vec<Plan> {
        // A page of 100 entries at the volume's referrals: half referred,
        // half payments just before a referral of their block.
        let mut keys = LinkKeys::default();
        for i in 1..=50 {
            keys.referred.chains.push(1);
            keys.referred.blocks.push(i * 97);
            keys.referred.logs.push(3);
            keys.paid.chains.push(1);
            keys.paid.blocks.push(i * 89);
            keys.paid.logs.push(1);
            keys.nodes.push(B256::repeat_byte(3).to_vec());
            keys.senders.push(Address::repeat_byte(0x5A).to_vec());
        }
        let name = "a history page's referral links";
        let text: Vec<(String,)> = keys
            .query(&explain(&LINKS))
            .fetch_all(&mut *self)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        vec![Plan {
            name,
            seeks: Some(&[
                "referrals_pkey:chain_id,block_number",
                "published_pkey",
                "handle_nodes_node_chain_idx:handle_node",
            ]),
            text: text
                .into_iter()
                .map(|(line,)| line)
                .collect::<Vec<_>>()
                .join("\n"),
        }]
    }
}
