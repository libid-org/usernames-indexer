//! The awaiting reads, planned.

use alloy::primitives::Address;
use usernames_core::db::{
    Page,
    SlotCursor,
};

use alloy::primitives::B256;
use sqlx::PgConnection;

use crate::Plan;

/// What waits for a holder, planned.
pub(crate) trait AwaitingPlans {
    /// The awaiting read, in every shape it is asked in.
    async fn awaiting_plans(&mut self) -> Vec<Plan>;
}

impl AwaitingPlans for PgConnection {
    async fn awaiting_plans(&mut self) -> Vec<Plan> {
        // An address the volume holds nothing for, and one holding many
        // handles on two chains.
        let (holder, many) = (Address::repeat_byte(2), Address::repeat_byte(0x4E));
        // A cursor high in the node and token ranges, where a list read
        // backwards has most of its rows still ahead of it.
        let before = Some(SlotCursor {
            chain_id: 1,
            handle_node: B256::repeat_byte(0xEE),
            token: Address::repeat_byte(0xEE),
        });
        // The holder's handles drive the page through the owner index;
        // each one's slots are looked up by its node, and whether anybody
        // holds the node on a slot's chain by the handles' key.
        let seeks: &[&str] = &[
            "handles_owner_chain_node_idx",
            "escrow_held_node_idx|escrow_held_pkey",
            "handles_pkey|handles_node_chain_idx",
            "handle_nodes_node_chain_idx",
        ];
        let mut plans = Vec::new();
        for (name, holder, chain, before) in [
            ("awaiting", holder, None, None),
            ("awaiting, past a cursor", holder, None, before),
            (
                "awaiting, one chain, past a cursor",
                holder,
                Some(1),
                before,
            ),
            ("awaiting many handles", many, None, None),
            ("awaiting many handles, past a cursor", many, None, before),
            (
                "awaiting many handles, one chain, past a cursor",
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
            let statement = page.awaiting_lookup("EXPLAIN ", holder);
            let plan = Plan::of(self, name, Some(seeks), statement).await;
            plans.push(plan.reading_once(holder));
        }
        plans
    }
}
