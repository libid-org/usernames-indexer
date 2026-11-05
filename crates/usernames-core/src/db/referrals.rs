//! Referrals: each `Referred` the splitter logs, with where the escrow put a
//! paid share, and the history of the referrals a holder's handles earned.

use alloy::primitives::{
    Address,
    B256,
    U256,
};
use sqlx::QueryBuilder;

use super::{
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
    Delivery,
    NamesEvent,
    Outcome,
};

/// The splitter's last deposit in a window: the paid share whose `Referred`
/// comes after it in the same transaction.
pub(crate) struct Deposit {
    tx_hash: B256,
    handle_node: B256,
    delivery: Delivery,
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
    pub(crate) fn delivery(
        &mut self,
        event: &NamesEvent,
        at: &Position,
    ) -> Result<Option<Delivery>, ApplyError> {
        match event {
            NamesEvent::Escrow(escrow) => {
                let deposit = self
                    .splitter
                    .and_then(|splitter| escrow.delivery_from(splitter));
                if let Some((handle_node, delivery)) = deposit {
                    self.deposit = Some(Deposit {
                        tx_hash: at.tx_hash,
                        handle_node,
                        delivery,
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
                    deposit.tx_hash == at.tx_hash && deposit.handle_node == *referrer_node
                })
                .map(|deposit| Some(deposit.delivery))
                .ok_or(ApplyError::UnpairedReferral {
                    block: at.block,
                    log_index: at.log_index,
                }),
            _ => Ok(None),
        }
    }

    /// Project a `Referred` into the referrer's history, with where the
    /// escrow put a paid share.
    pub(crate) async fn book_referral(
        &mut self,
        event: &NamesEvent,
        delivery: Option<Delivery>,
        at: &Position,
    ) -> Result<(), ApplyError> {
        let NamesEvent::Referred {
            referrer_node,
            binder,
            referrer_platform_id,
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
            .bind(referrer_platform_id)
            .bind(binder)
            .bind(platform_id)
            .bind(handle_node)
            .bind(fee.to_string())
            .bind(share.to_string())
            .bind(outcome.as_str())
            .bind(delivery.map(Delivery::as_str))
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
    /// The referrer's platform.
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
        let mut statement =
            QueryBuilder::new(format!("{prefix}{}", sql::REFERRALS_BY_NODE));
        self.bound(&mut statement, "r");
        statement
            .push(") r\n")
            .push(sql::REFERRALS_HOLDER)
            .push_bind(holder)
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
    use crate::db::plan_tests::Plan;

    /// A holder's referrals, planned.
    pub(crate) trait ReferralHistoryPlans {
        /// The referral history, in every shape it is asked in.
        async fn referral_history_plans(&mut self) -> Vec<Plan>;
    }

    impl ReferralHistoryPlans for PgConnection {
        async fn referral_history_plans(&mut self) -> Vec<Plan> {
            let holder = Address::repeat_byte(2);
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
            for (name, chain, before) in [
                ("referrals", None, None),
                ("referrals, past a cursor", None, before),
                ("referrals, one chain, past a cursor", Some(1), before),
            ] {
                let page = Page {
                    chain,
                    before,
                    limit: 20,
                };
                let statement = page.referrals_lookup("EXPLAIN ", holder);
                plans.push(Plan::of(self, name, Some(seeks), statement).await);
            }
            plans
        }
    }
}
