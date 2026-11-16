//! The referral reads, planned.

use alloy::primitives::B256;
use usernames_core::{
    db::sql,
    referral::Code,
};

use sqlx::PgConnection;

use crate::{
    explain,
    Plan,
};

/// The referral codes' reads, planned.
pub(crate) trait ReferralPlans {
    /// A code's owner, in the middle of the node range and at its top,
    /// and a referrer.
    async fn referral_plans(&mut self) -> Vec<Plan>;
}

impl ReferralPlans for PgConnection {
    async fn referral_plans(&mut self) -> Vec<Plan> {
        let owner: &[&str] = &["handles_node_chain_idx", "events_binds_idx"];
        let mut plans = Vec::new();
        for (name, node) in [
            ("a code's owner", B256::repeat_byte(0x7c)),
            ("a code's owner, all 0xff", B256::repeat_byte(0xff)),
        ] {
            let code = Code::prefixes(node).next().expect("a shortest code");
            plans.push(
                Plan::of(self, name, Some(owner), code.owner_lookup("EXPLAIN ")).await,
            );
        }
        let text: Vec<String> = sqlx::query_scalar(&explain(sql::REFERRAL_CODE_REFERRER))
            .bind(B256::repeat_byte(3))
            .fetch_all(&mut *self)
            .await
            .expect("a referrer's plan");
        plans.push(Plan {
            name: "a referrer",
            seeks: Some(&[
                "handles_node_chain_idx",
                "published_pkey",
                "handle_nodes_pkey|handle_nodes_node_chain_idx",
            ]),
            text: text.join("\n"),
        });
        plans
    }
}
