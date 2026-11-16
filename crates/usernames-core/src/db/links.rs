//! The referral behind a history entry: a `referred` entry's own, and the one
//! a deposit or forward from the chain's referral splitter paid.

use std::{
    collections::HashMap,
    sync::LazyLock,
};

use alloy::primitives::{
    Address,
    B256,
};
use sqlx::{
    postgres::{
        PgArguments,
        PgRow,
    },
    query::QueryAs,
    FromRow,
    Postgres,
};

use super::{
    reads::HandleText,
    sql,
    HistoryRow,
    Store,
};
use crate::events::EventKind;

/// [`sql::HISTORY_REFERRAL_LINKS`] with the referrer handle's text.
pub static LINKS: LazyLock<String> = LazyLock::new(|| {
    let text = HandleText {
        node: "l.referrer_node",
        alias: "rv",
    };
    format!("{}{text}", sql::HISTORY_REFERRAL_LINKS)
});

/// The referral a history entry links to, under the entry's key.
#[derive(sqlx::FromRow)]
pub struct ReferralLinkRow {
    /// The entry's chain.
    pub chain_id: i64,
    /// The entry's block.
    pub block_number: i64,
    /// The entry's index in its block.
    pub log_index: i64,
    /// The `Referred`'s index in the same block.
    pub referral_log_index: i64,
    /// The referrer handle's node.
    pub referrer_node: B256,
    /// The text any chain disclosed for it, read from the lowest chain id.
    pub referrer_handle: Option<String>,
    /// Who bound.
    pub binder: Address,
    /// The binder's published handle on the entry's chain, as it stands.
    pub binder_handle: Option<String>,
}

/// The keys of a history page's entries, as one statement's arrays.
#[derive(Default)]
pub struct EntryKeys {
    /// The chain of each entry.
    pub chains: Vec<i64>,
    /// Its block.
    pub blocks: Vec<i64>,
    /// Its log index in the block.
    pub logs: Vec<i64>,
}

impl EntryKeys {
    fn push(&mut self, row: &HistoryRow) {
        self.chains.push(row.chain_id);
        self.blocks.push(row.block_number);
        self.logs.push(row.log_index);
    }
}

/// The entries of a history page that may link to a referral: `referred`
/// entries, and deposits and forwards with the node they paid and who sent
/// them. The statement keeps the payments the chain's splitter sent.
#[derive(Default)]
pub struct LinkKeys {
    /// The `referred` entries.
    pub referred: EntryKeys,
    /// The deposits and forwards that paid a node.
    pub paid: EntryKeys,
    /// The node each of those paid.
    pub nodes: Vec<Vec<u8>>,
    /// Who sent each of them.
    pub senders: Vec<Vec<u8>>,
}

impl LinkKeys {
    fn of(rows: &[HistoryRow]) -> Self {
        let mut keys = Self::default();
        for row in rows {
            match row.event.kind.parse() {
                Ok(EventKind::Referred) => keys.referred.push(row),
                Ok(EventKind::Deposited | EventKind::Forwarded) => {
                    if let (Some(node), Some(sender)) =
                        (row.event.handle_node, row.event.sender)
                    {
                        keys.paid.push(row);
                        keys.nodes.push(node.to_vec());
                        keys.senders.push(sender.to_vec());
                    }
                }
                _ => {}
            }
        }
        keys
    }

    fn is_empty(&self) -> bool {
        self.referred.chains.is_empty() && self.paid.chains.is_empty()
    }

    /// `text`, the links statement or its plan, bound to these keys.
    pub fn query<'q, O>(&'q self, text: &'q str) -> QueryAs<'q, Postgres, O, PgArguments>
    where
        O: for<'r> FromRow<'r, PgRow>,
    {
        sqlx::query_as(text)
            .bind(&self.referred.chains)
            .bind(&self.referred.blocks)
            .bind(&self.referred.logs)
            .bind(&self.paid.chains)
            .bind(&self.paid.blocks)
            .bind(&self.paid.logs)
            .bind(&self.nodes)
            .bind(&self.senders)
    }
}

/// The referrals a history page's entries link to, by entry key.
#[derive(Default)]
pub struct ReferralLinks(HashMap<(i64, i64, i64), ReferralLinkRow>);

impl ReferralLinks {
    /// The referral `row` links to.
    pub fn take(&mut self, row: &HistoryRow) -> Option<ReferralLinkRow> {
        self.0
            .remove(&(row.chain_id, row.block_number, row.log_index))
    }
}

impl Store {
    /// The referrals behind `rows`' `referred` entries and the payments the
    /// referral splitter sent. A page with neither reads nothing.
    pub async fn referral_links(
        &self,
        rows: &[HistoryRow],
    ) -> Result<ReferralLinks, sqlx::Error> {
        let keys = LinkKeys::of(rows);
        if keys.is_empty() {
            return Ok(ReferralLinks::default());
        }
        let links: Vec<ReferralLinkRow> =
            keys.query(&LINKS).fetch_all(&self.pool).await?;
        Ok(ReferralLinks(
            links
                .into_iter()
                .map(|link| ((link.chain_id, link.block_number, link.log_index), link))
                .collect(),
        ))
    }
}
