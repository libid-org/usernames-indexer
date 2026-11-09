//! Referrals: each `Referred` the splitter logs, with where the escrow put a
//! paid share, and the history of the referrals a holder's handles earned.

use super::{
    holder_nodes,
    sql,
    ApplyError,
    Cursor,
    HistoryCursor,
    Page,
    Position,
    Rows,
    Statement,
    Store,
    Window,
};
use crate::events::{
    EscrowEvent,
    NamesEvent,
    Outcome,
    ShareMove,
};
use alloy::primitives::{
    Address,
    B256,
    U256,
};

/// The splitter's last deposit in a window: the paid share whose `Referred`
/// comes after it in the same transaction.
pub(crate) struct Deposit {
    tx_hash: B256,
    share: ShareMove,
}

impl Window {
    /// Where the escrow put the share `event` reports, once it is a paid
    /// referral: its deposit is the splitter's last one before it, in its
    /// transaction and for the referrer's node. A splitter deposit is
    /// remembered here for the referral after it. Called for every event,
    /// replayed ones included, before the journal decides whether it is new.
    ///
    /// A paid referral without that deposit fails the window: the splitter
    /// checked at startup never logs one, so only another contract at its
    /// address could.
    pub(crate) fn deposit(
        &mut self,
        event: &NamesEvent,
        at: &Position,
    ) -> Result<Option<ShareMove>, ApplyError> {
        match event {
            NamesEvent::Escrow(escrow) => {
                let share = self
                    .splitter
                    .and_then(|splitter| escrow.deposit_by(splitter));
                if let Some(share) = share {
                    self.deposit = Some(Deposit {
                        tx_hash: at.tx_hash,
                        share,
                    });
                }
                Ok(None)
            }
            NamesEvent::Referred {
                referrer_node,
                outcome: Outcome::Paid,
                ..
            } => self
                .deposit
                .take()
                .filter(|deposit| {
                    deposit.tx_hash == at.tx_hash
                        && deposit.share.handle_node == *referrer_node
                })
                .map(|deposit| Some(deposit.share))
                .ok_or(ApplyError::UnpairedReferral {
                    block: at.block,
                    log_index: at.log_index,
                }),
            _ => Ok(None),
        }
    }

    /// Project a `Referred` into the referrer's history, with the deposit
    /// that took a paid share.
    pub(crate) async fn book_referral(
        &mut self,
        event: &NamesEvent,
        deposit: Option<ShareMove>,
        at: &Position,
    ) -> Result<(), ApplyError> {
        let NamesEvent::Referred {
            referrer_node,
            binder,
            platform_id,
            handle_node,
            fee,
            share,
            outcome,
            ..
        } = event
        else {
            return Ok(());
        };
        sqlx::query(sql::RECORD_REFERRAL)
            .bind(self.chain_id)
            .bind(at.block)
            .bind(at.log_index)
            .bind(at.block_time)
            .bind(referrer_node)
            .bind(binder)
            .bind(platform_id)
            .bind(handle_node)
            .bind(fee.to_string())
            .bind(share.to_string())
            .bind(outcome.as_str())
            .bind(deposit.map(|deposit| deposit.delivery.as_str()))
            .bind(deposit.map(|deposit| deposit.token))
            .bind(
                deposit
                    .and_then(|deposit| deposit.round)
                    .map(|round| round.to_string()),
            )
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Mark the shares a claim, or the splitter's refund, took out of a slot's
    /// round as claimed or refunded.
    pub(crate) async fn settle_referrals(
        &mut self,
        escrow: &EscrowEvent,
    ) -> Result<(), ApplyError> {
        let Some(settled) = self
            .splitter
            .and_then(|splitter| escrow.settles_for(splitter))
        else {
            return Ok(());
        };
        sqlx::query(sql::SETTLE_REFERRALS)
            .bind(self.chain_id)
            .bind(settled.handle_node)
            .bind(settled.token)
            .bind(settled.round.map(|round| round.to_string()))
            .bind(settled.delivery.as_str())
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }
}

/// One referral as the store holds it, with the binder's published handle.
#[derive(Debug, sqlx::FromRow)]
pub struct ReferralRow {
    /// The chain the bind happened on.
    pub chain_id: i64,
    /// Its block.
    pub block_number: i64,
    /// Its log's index in the block.
    pub log_index: i64,
    /// Its block's time, in unix seconds.
    pub block_time: i64,
    /// The referrer's handle node, which the share is for.
    pub referrer_node: B256,
    /// The platform the referrer's node is bound on.
    pub referrer_platform: B256,
    /// The wallet that bound.
    pub binder: Address,
    /// The platform of the handle bound.
    pub platform_id: B256,
    /// The handle bound.
    pub handle_node: B256,
    /// The bind's fee, as Postgres prints a `numeric`.
    pub fee: String,
    /// The referrer's share of it, likewise.
    pub share: String,
    /// What became of the share, as stored.
    pub outcome: String,
    /// Where the escrow put a paid share, as stored.
    pub delivery: Option<String>,
    /// The referrer handle's disclosed text.
    pub referrer_handle: Option<String>,
    /// The binder's published handle on the referral's chain, as it stands.
    pub binder_handle: Option<String>,
}

impl ReferralRow {
    /// The fee as the chain counts it; `None` only for a value no build
    /// writes.
    pub fn fee(&self) -> Option<U256> {
        self.fee.parse().ok()
    }

    /// The share, under the same rule.
    pub fn share(&self) -> Option<U256> {
        self.share.parse().ok()
    }
}

impl Cursor<ReferralRow> for HistoryCursor {
    fn after(row: &ReferralRow) -> Option<Self> {
        Some(Self {
            block_time: row.block_time,
            chain_id: row.chain_id,
            block_number: row.block_number,
            log_index: row.log_index,
        })
    }
}

impl Page<HistoryCursor> {
    /// `holder`'s referrals: a page's worth per node of the handles it
    /// holds, then the page across them, newest first.
    pub(crate) fn referrals_lookup(&self, prefix: &str, holder: Address) -> Statement {
        let mut statement = holder_nodes(prefix, holder, None);
        statement.push(sql::REFERRALS_BY_NODE);
        self.bound(&mut statement, "r");
        statement
            .push(") r\n")
            .push(sql::REFERRALS_HOLDER)
            .push(
                " ORDER BY r.block_time DESC, r.chain_id DESC, r.block_number DESC, \
                 r.log_index DESC LIMIT ",
            )
            .push_bind(self.fetch());
        statement
    }
}

impl Store {
    /// The referrals the handles `holder` holds earned, newest first. A
    /// referral names a node, so it counts on every chain, once per node.
    pub async fn referrals(
        &self,
        holder: Address,
        page: Page<HistoryCursor>,
    ) -> Result<Rows<ReferralRow, HistoryCursor>, sqlx::Error> {
        let rows = page
            .referrals_lookup("", holder)
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }
}

#[cfg(test)]
pub(crate) mod plans {
    use sqlx::PgConnection;

    use super::*;
    use crate::{
        db::plan_tests::{
            explain,
            Plan,
        },
        events::Delivery,
    };

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
            let seeks: &[&str] =
                &["handles_owner_chain_node_idx", "referrals_referrer_idx"];
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
                plans.push(plan.reading_once(holder));
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
}
