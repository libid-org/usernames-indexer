//! The escrow reads, planned.

use alloy::primitives::{
    Address,
    B256,
    U256,
};
use usernames_core::db::{
    page::Page,
    sql,
};

use sqlx::PgConnection;

use crate::{
    explain,
    Plan,
};
use usernames_core::db::escrow::*;

fn page<C>(chain: Option<i64>, before: Option<C>) -> Page<C> {
    Page {
        chain,
        before,
        limit: 20,
    }
}
/// The escrow's reads, planned.
pub(crate) trait EscrowPlans {
    /// The escrow's lists and the books' writes, in every shape they are asked in.
    async fn escrow_plans(&mut self) -> Vec<Plan>;
}

impl EscrowPlans for PgConnection {
    async fn escrow_plans(&mut self) -> Vec<Plan> {
        let (address, node) = (Address::repeat_byte(2), B256::repeat_byte(1));
        // A cursor high in the node, token and amount ranges, where a list
        // read backwards has most of its rows still ahead of it.
        let (high, token) = (B256::repeat_byte(0xEE), Address::repeat_byte(0xEE));
        let slot = Some(SlotCursor {
            chain_id: 1,
            handle_node: high,
            token,
        });
        let unclaimed = Some(UnclaimedCursor {
            token,
            held: U256::from(4000u64),
            chain_id: 1,
            handle_node: high,
        });
        // The holder's handles drive the page through the owner index, and
        // each one's slots are looked up by its node, and each slot's text
        // by node on any chain.
        let claimable: &[&str] = &[
            "handles_owner_chain_node_idx",
            "escrow_held_node_idx|escrow_held_pkey",
            "handle_nodes_node_chain_idx",
            "handle_node = h.handle_node",
        ];
        let refundable: &[&str] = &["escrow_refundable_pkey"];
        let slots: &[&str] = &["escrow_held_node_idx"];
        // One chain and one node: the key seeks the page exactly as well.
        let slots_on_one: &[&str] = &["escrow_held_node_idx|escrow_held_pkey"];
        let across: &[&str] = &["escrow_held_unclaimed_idx"];
        let one: &[&str] = &["escrow_held_chain_unclaimed_idx"];
        let mut plans = Vec::new();
        for (name, seeks, statement) in [
            (
                "claimable",
                claimable,
                page(None, None).claimable_lookup("EXPLAIN ", address),
            ),
            (
                "claimable, past a cursor",
                claimable,
                page(None, slot).claimable_lookup("EXPLAIN ", address),
            ),
            (
                "claimable, one chain, past a cursor",
                claimable,
                page(Some(1), slot).claimable_lookup("EXPLAIN ", address),
            ),
            (
                "refundable",
                refundable,
                page(None, None).refundable_lookup("EXPLAIN ", address),
            ),
            (
                "refundable, past a cursor",
                refundable,
                page(None, slot).refundable_lookup("EXPLAIN ", address),
            ),
            (
                "refundable, one chain, past a cursor",
                refundable,
                page(Some(1), slot).refundable_lookup("EXPLAIN ", address),
            ),
            (
                "a node's slots",
                slots,
                page(None, None).slots_lookup("EXPLAIN ", node),
            ),
            (
                "a node's slots, past a cursor",
                slots,
                page(None, slot).slots_lookup("EXPLAIN ", node),
            ),
            (
                "a node's slots, one chain, past a cursor",
                slots_on_one,
                page(Some(1), slot).slots_lookup("EXPLAIN ", node),
            ),
            (
                "unclaimed",
                across,
                page(None, None).unclaimed_lookup("EXPLAIN ", None),
            ),
            (
                "unclaimed, past a cursor",
                across,
                page(None, unclaimed).unclaimed_lookup("EXPLAIN ", None),
            ),
            (
                "unclaimed in one token",
                across,
                page(None, unclaimed).unclaimed_lookup("EXPLAIN ", Some(token)),
            ),
            (
                "unclaimed, one chain",
                one,
                page(Some(1), None).unclaimed_lookup("EXPLAIN ", None),
            ),
            (
                "unclaimed, one chain, past a cursor",
                one,
                page(Some(1), unclaimed).unclaimed_lookup("EXPLAIN ", None),
            ),
            (
                "unclaimed in one token, one chain",
                one,
                page(Some(1), unclaimed).unclaimed_lookup("EXPLAIN ", Some(token)),
            ),
        ] {
            plans.push(Plan::of(self, name, Some(seeks), statement).await);
        }

        let writes = [
            (
                "a claim",
                &["escrow_held_pkey"][..],
                sqlx::query_scalar::<_, String>(&explain(sql::ESCROW_CLAIM))
                    .bind(1i64)
                    .bind(node)
                    .bind(token)
                    .bind("0")
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a claim closing its round",
                &["escrow_refundable_slot_idx"][..],
                sqlx::query_scalar(&explain(sql::ESCROW_CLOSE_ROUND))
                    .bind(1i64)
                    .bind(node)
                    .bind(token)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a refund",
                &["escrow_held_pkey"][..],
                sqlx::query_scalar(&explain(sql::ESCROW_REFUND))
                    .bind(1i64)
                    .bind(node)
                    .bind(token)
                    .bind("1")
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a refund taking its contribution",
                &["escrow_refundable_pkey|escrow_refundable_slot_idx"][..],
                sqlx::query_scalar(&explain(sql::ESCROW_TAKE_REFUNDABLE))
                    .bind(1i64)
                    .bind(node)
                    .bind(token)
                    .bind(address)
                    .fetch_all(&mut *self)
                    .await,
            ),
        ];
        for (name, seeks, text) in writes {
            let text: Vec<String> = text.unwrap_or_else(|e| panic!("{name}: {e}"));
            plans.push(Plan {
                name,
                seeks: Some(seeks),
                text: text.join("\n"),
            });
        }
        plans
    }
}
