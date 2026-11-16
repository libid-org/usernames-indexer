//! The referrals reads, planned.

use crate::{
    explain,
    Plan,
};
use alloy::primitives::{
    Address,
    B256,
};
use usernames_core::db::{
    sql,
    HistoryCursor,
    Page,
};

use sqlx::PgConnection;

use usernames_core::events::Delivery;

/// A holder's referrals, planned.
pub(crate) trait ReferralHistoryPlans {
    /// The referral history, in every shape it is asked in, and the write
    /// that settles held referrals.
    async fn referral_history_plans(&mut self) -> Vec<Plan>;
}

impl ReferralHistoryPlans for PgConnection {
    async fn referral_history_plans(&mut self) -> Vec<Plan> {
        // An address the volume holds nothing for, and one holding many
        // handles on two chains.
        let (holder, many) = (Address::repeat_byte(2), Address::repeat_byte(0x4E));
        // A cursor late in the volume's times, where a history read
        // backwards has most of its rows still ahead of it.
        let before = Some(HistoryCursor {
            block_time: 4000,
            chain_id: 1,
            block_number: 4000,
            log_index: 3,
        });
        // The holder's handles drive the page through the owner index,
        // and each node's referrals are read backwards from its index.
        let seeks: &[&str] = &[
            "handles_owner_chain_node_idx",
            "referrals_referrer_idx",
            "events_pkey",
            "handle_nodes_node_chain_idx",
        ];
        let mut plans = Vec::new();
        for (name, holder, chain, before) in [
            ("referrals", holder, None, None),
            ("referrals, past a cursor", holder, None, before),
            (
                "referrals, one chain, past a cursor",
                holder,
                Some(1),
                before,
            ),
            ("referrals of many handles", many, None, None),
            (
                "referrals of many handles, past a cursor",
                many,
                None,
                before,
            ),
            (
                "referrals of many handles, one chain, past a cursor",
                many,
                Some(1),
                before,
            ),
        ] {
            let page = Page {
                chain,
                before,
                limit: 20,
            };
            let statement = page.referrals_lookup("EXPLAIN ", holder);
            let plan = Plan::of(self, name, Some(seeks), statement).await;
            plans.push(plan.reading_once(holder).cut_before("events_pkey"));
        }

        let name = "a claim settling its round's held referrals";
        let settle: Vec<String> = sqlx::query_scalar(&explain(sql::SETTLE_REFERRALS))
            .bind(1i64)
            .bind(B256::repeat_byte(1))
            .bind(Address::repeat_byte(0xEE))
            .bind("0")
            .bind(Delivery::Claimed.as_str())
            .fetch_all(&mut *self)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        plans.push(Plan {
            name,
            seeks: Some(&["referrals_held_idx"]),
            text: settle.join("\n"),
        });
        plans
    }
}
