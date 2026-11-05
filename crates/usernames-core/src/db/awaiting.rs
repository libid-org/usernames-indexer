//! What waits for a holder on the chains where nobody holds its handles:
//! escrowed for a node it holds elsewhere, claimable once it binds there.

use alloy::primitives::Address;
use sqlx::QueryBuilder;

use super::{
    sql,
    EscrowSlotRow,
    Page,
    Rows,
    SlotCursor,
    Statement,
    Store,
};

impl Page<SlotCursor> {
    /// What waits for `holder`: node by node of the handles it holds, each
    /// node's slots by chain and token, descending.
    pub(crate) fn awaiting_lookup(&self, prefix: &str, holder: Address) -> Statement {
        let mut statement =
            QueryBuilder::new(format!("{prefix}{}", sql::ESCROW_AWAITING_SLOTS));
        if let Some(chain) = self.chain {
            statement.push(" AND e.chain_id = ").push_bind(chain);
        }
        // The node leads the comparison as it leads the node index, and is
        // the lateral's own node: past the cursor's node every slot passes.
        if let Some(before) = self.before {
            statement
                .push(" AND (e.handle_node, e.chain_id, e.token) < (")
                .push_bind(before.handle_node)
                .push(", ")
                .push_bind(before.chain_id)
                .push(", ")
                .push_bind(before.token)
                .push(")");
        }
        statement
            .push(" ORDER BY e.handle_node DESC, e.chain_id DESC, e.token DESC LIMIT ")
            .push_bind(self.fetch())
            .push(") e\n")
            .push(sql::ESCROW_AWAITING_HOLDER)
            .push_bind(holder);
        if let Some(before) = self.before {
            statement
                .push(" AND h.handle_node <= ")
                .push_bind(before.handle_node);
        }
        statement
            .push(" ORDER BY e.handle_node DESC, e.chain_id DESC, e.token DESC LIMIT ")
            .push_bind(self.fetch());
        statement
    }
}

impl Store {
    /// What waits for a holder: every slot holding something for a handle
    /// node it holds on some chain, on each chain where nobody holds that
    /// node. Binding the handle on that chain makes the slot its claim.
    pub async fn awaiting(
        &self,
        holder: Address,
        page: Page<SlotCursor>,
    ) -> Result<Rows<EscrowSlotRow, SlotCursor>, sqlx::Error> {
        let rows = page
            .awaiting_lookup("", holder)
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }
}

#[cfg(test)]
pub(crate) mod plans {
    use alloy::primitives::B256;
    use sqlx::PgConnection;

    use super::*;
    use crate::db::plan_tests::Plan;

    /// What waits for a holder, planned.
    pub(crate) trait AwaitingPlans {
        /// The awaiting read, in every shape it is asked in.
        async fn awaiting_plans(&mut self) -> Vec<Plan>;
    }

    impl AwaitingPlans for PgConnection {
        async fn awaiting_plans(&mut self) -> Vec<Plan> {
            let holder = Address::repeat_byte(2);
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
            ];
            let mut plans = Vec::new();
            for (name, chain, before) in [
                ("awaiting", None, None),
                ("awaiting, past a cursor", None, before),
                ("awaiting, one chain, past a cursor", Some(1), before),
            ] {
                let page = Page {
                    chain,
                    before,
                    limit: 20,
                };
                let statement = page.awaiting_lookup("EXPLAIN ", holder);
                plans.push(Plan::of(self, name, Some(seeks), statement).await);
            }
            plans
        }
    }
}
