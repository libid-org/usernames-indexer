//! The history reads, planned.

use alloy::primitives::{
    Address,
    B256,
};
use usernames_core::{
    db::{
        page::Page,
        sql,
    },
    events::EventKind,
};

use sqlx::PgConnection;

use crate::{
    explain,
    Plan,
};
use usernames_core::db::history::*;
/// The histories' reads, planned.
pub(crate) trait HistoryPlans {
    /// The histories' lookups and the indexer's reads, in every shape they are
    /// asked in.
    async fn history_plans(&mut self) -> Vec<Plan>;
}

impl HistoryPlans for PgConnection {
    async fn history_plans(&mut self) -> Vec<Plan> {
        let (address, node) = (Address::repeat_byte(2), B256::repeat_byte(1));
        let cursor = Some(HistoryCursor {
            block_time: 1_700_000_000,
            chain_id: 1,
            block_number: 10,
            log_index: 0,
        });
        let page = |chain, before| Page {
            chain,
            before,
            limit: 20,
        };
        let across: &[&str] = &["address_events_history_idx"];
        let one: &[&str] = &["address_events_pkey"];
        let handle: &[&str] = &["events_handle_history_idx"];
        let mut plans = Vec::new();
        for (name, seeks, statement) in [
            (
                "address history",
                across,
                page(None, None).address_lookup("EXPLAIN ", address),
            ),
            (
                "address history, past a cursor",
                across,
                page(None, cursor).address_lookup("EXPLAIN ", address),
            ),
            (
                "address history, one chain",
                one,
                page(Some(1), None).address_lookup("EXPLAIN ", address),
            ),
            (
                "address history, one chain, past a cursor",
                one,
                page(Some(1), cursor).address_lookup("EXPLAIN ", address),
            ),
            (
                "handle history",
                handle,
                page(None, None).node_lookup("EXPLAIN ", node),
            ),
            (
                "handle history, past a cursor",
                handle,
                page(None, cursor).node_lookup("EXPLAIN ", node),
            ),
            (
                "handle history, one chain",
                handle,
                page(Some(1), None).node_lookup("EXPLAIN ", node),
            ),
            (
                "handle history, one chain, past a cursor",
                handle,
                page(Some(1), cursor).node_lookup("EXPLAIN ", node),
            ),
        ] {
            plans.push(Plan::of(self, name, Some(seeks), statement).await);
        }

        let on_chain = ChainEvents {
            chain_id: 1,
            day: None,
            kind: None,
            token: None,
        };
        let day = Some(0);
        let deposits = Some(EventKind::Deposited);
        let token = Some(Address::repeat_byte(5));
        let time: &[&str] = &["events_chain_time_idx"];
        let kind: &[&str] = &["events_chain_kind_time_idx"];
        let in_token: &[&str] = &["events_chain_token_time_idx"];
        let token_kind: &[&str] = &["events_chain_token_kind_time_idx"];
        for (name, past, seeks, events) in [
            (
                "chain history",
                "chain history, past a cursor",
                time,
                on_chain,
            ),
            (
                "chain history, one day",
                "chain history, one day, past a cursor",
                time,
                ChainEvents { day, ..on_chain },
            ),
            (
                "chain history, one kind",
                "chain history, one kind, past a cursor",
                kind,
                ChainEvents {
                    kind: deposits,
                    ..on_chain
                },
            ),
            (
                "chain history, one day, one kind",
                "chain history, one day, one kind, past a cursor",
                kind,
                ChainEvents {
                    day,
                    kind: deposits,
                    ..on_chain
                },
            ),
            (
                "chain history, one token",
                "chain history, one token, past a cursor",
                in_token,
                ChainEvents { token, ..on_chain },
            ),
            (
                "chain history, one day, one token",
                "chain history, one day, one token, past a cursor",
                in_token,
                ChainEvents {
                    day,
                    token,
                    ..on_chain
                },
            ),
            (
                "chain history, one kind, one token",
                "chain history, one kind, one token, past a cursor",
                token_kind,
                ChainEvents {
                    kind: deposits,
                    token,
                    ..on_chain
                },
            ),
            (
                "chain history, one day, one kind, one token",
                "chain history, one day, one kind, one token, past a cursor",
                token_kind,
                ChainEvents {
                    day,
                    kind: deposits,
                    token,
                    ..on_chain
                },
            ),
        ] {
            for (name, before) in [(name, None), (past, cursor)] {
                let statement = page(None, before).chain_lookup("EXPLAIN ", events);
                plans.push(Plan::of(self, name, Some(seeks), statement).await);
            }
        }

        let reads = [
            (
                "previous holders",
                &[
                    "handles_pkey|handles_node_chain_idx",
                    "ids_pkey|ids_node_chain_idx",
                ][..],
                sqlx::query_scalar::<_, String>(&explain(sql::PREVIOUS_HOLDERS))
                    .bind(1i64)
                    .bind(node)
                    .bind(node)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "the senders a claim takes from",
                &["escrow_refundable_slot_idx"][..],
                sqlx::query_scalar(&explain(sql::CLAIMED_SENDERS))
                    .bind(1i64)
                    .bind(node)
                    .bind(address)
                    .fetch_all(&mut *self)
                    .await,
            ),
        ];
        for (name, seeks, text) in reads {
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
