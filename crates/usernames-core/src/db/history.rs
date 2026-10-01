//! The histories: which addresses and which handle each journal row involves,
//! written beside it, and the reads that list either newest first.
//!
//! An event names some of what it involves and not all of it. A bind does
//! not name the address it takes a handle or an account from, and a
//! retirement names the binding wallet rather than the one that held the
//! handle; a ceremony, a fee and an unpublish name no handle; a fee does not
//! name who paid it; a claim does not name whose deposits it took. Each of
//! those is read here from the rows the event is about to change, or from the
//! rows its own transaction recorded just before it.

use std::str::FromStr;

use alloy::primitives::{
    Address,
    B256,
};
use sqlx::QueryBuilder;

use super::{
    as_i64,
    scoped,
    sql,
    ApplyError,
    Statement,
    Store,
    Window,
};
use crate::events::{
    LogPosition,
    NamesEvent,
    Role,
};

/// What one event involves: the addresses, each with every part it plays in
/// it, and the handle node it concerns with that node's platform.
pub(super) struct Involvement {
    parties: Vec<(Address, Vec<Role>)>,
    handle: Option<(B256, B256)>,
}

impl Involvement {
    /// One address plays one more part. A payer that refunds to itself is one
    /// party with two roles, so its history lists the deposit once.
    fn add(&mut self, address: Address, role: Role) {
        match self.parties.iter_mut().find(|(party, _)| *party == address) {
            Some((_, roles)) if roles.contains(&role) => {}
            Some((_, roles)) => roles.push(role),
            None => self.parties.push((address, vec![role])),
        }
    }
}

/// The ceremony a bind fee was paid beside: the holder it recorded, and the
/// handle recorded for it.
#[derive(sqlx::FromRow)]
struct FeeCeremony {
    address: Vec<u8>,
    handle_node: Option<Vec<u8>>,
    platform_id: Option<Vec<u8>>,
}

impl Window {
    /// What `event` involves, read before its projections move.
    pub(super) async fn involvement(
        &mut self,
        event: &NamesEvent,
        pos: &LogPosition,
    ) -> Result<Involvement, ApplyError> {
        let chain_id = self.chain_id;
        let block = as_i64(pos.block_number, "block number")?;
        let log_index = as_i64(pos.log_index, "log index")?;
        let mut involvement = Involvement {
            parties: Vec::new(),
            handle: None,
        };
        for (address, role) in event.parties() {
            involvement.add(address, role);
        }

        match event {
            NamesEvent::IdentityBound {
                holder,
                id_node,
                handle_node,
                platform_id,
                ..
            } => {
                let (handle_holder, id_holder): (Option<Vec<u8>>, Option<Vec<u8>>) =
                    sqlx::query_as(sql::PREVIOUS_HOLDERS)
                        .bind(chain_id)
                        .bind(handle_node.as_slice())
                        .bind(id_node.as_slice())
                        .fetch_one(&mut *self.tx)
                        .await?;
                let previous = [
                    (handle_holder, Role::PreviousHandleHolder),
                    (id_holder, Role::PreviousIdHolder),
                ];
                for (bytes, role) in previous {
                    let previous =
                        bytes.and_then(|b| Address::try_from(b.as_slice()).ok());
                    if let Some(previous) = previous.filter(|p| p != holder) {
                        involvement.add(previous, role);
                    }
                }
                involvement.handle = Some((*handle_node, *platform_id));
            }

            // The event names the wallet whose bind retired the handle. When
            // the account moved wallets in that bind, the handle was held by
            // the one it left, and that is whose history it belongs in.
            NamesEvent::HandleRetired {
                platform_id,
                handle_node,
                holder,
            } => {
                let held_by: Option<Option<Vec<u8>>> =
                    sqlx::query_scalar(sql::HANDLE_HOLDER)
                        .bind(chain_id)
                        .bind(handle_node.as_slice())
                        .fetch_optional(&mut *self.tx)
                        .await?;
                let held_by = held_by
                    .flatten()
                    .and_then(|b| Address::try_from(b.as_slice()).ok());
                match held_by {
                    Some(held_by) if held_by != *holder => {
                        involvement.add(held_by, Role::PreviousHandleHolder)
                    }
                    _ => involvement.add(*holder, Role::Holder),
                }
                involvement.handle = Some((*handle_node, *platform_id));
            }

            NamesEvent::HandleUnpublished {
                holder,
                platform_id,
            } => {
                let node: Option<Vec<u8>> =
                    sqlx::query_scalar(sql::PUBLISHED_HANDLE_NODE)
                        .bind(chain_id)
                        .bind(holder.as_slice())
                        .bind(platform_id.as_slice())
                        .fetch_optional(&mut *self.tx)
                        .await?;
                // Nothing published is still a withdrawal the contract
                // accepts; it concerns no handle.
                involvement.handle =
                    node.map(|node| (B256::from_slice(&node), *platform_id));
            }

            NamesEvent::CeremonyBound {
                holder,
                platform_id,
                ..
            } => {
                let node: Option<Vec<u8>> = sqlx::query_scalar(sql::CEREMONY_HANDLE)
                    .bind(chain_id)
                    .bind(block)
                    .bind(pos.tx_hash.as_slice())
                    .bind(log_index)
                    .bind(holder.as_slice())
                    .bind(Role::Holder.as_str())
                    .bind(platform_id.as_slice())
                    .fetch_optional(&mut *self.tx)
                    .await?;
                match node {
                    Some(node) => {
                        involvement.handle = Some((B256::from_slice(&node), *platform_id))
                    }
                    None => tracing::warn!(
                        block,
                        log_index,
                        "a ceremony without the bind beside it"
                    ),
                }
            }

            NamesEvent::BindFeePaid {
                authorization_digest,
                ..
            } => {
                let ceremony: Option<FeeCeremony> = sqlx::query_as(sql::FEE_PAYER)
                    .bind(chain_id)
                    .bind(block)
                    .bind(pos.tx_hash.as_slice())
                    .bind(authorization_digest.to_string())
                    .fetch_optional(&mut *self.tx)
                    .await?;
                match ceremony {
                    Some(ceremony) => {
                        if let Ok(holder) = Address::try_from(ceremony.address.as_slice())
                        {
                            involvement.add(holder, Role::Holder);
                        }
                        if let (Some(node), Some(platform)) =
                            (ceremony.handle_node, ceremony.platform_id)
                        {
                            involvement.handle = Some((
                                B256::from_slice(&node),
                                B256::from_slice(&platform),
                            ));
                        }
                    }
                    None => tracing::warn!(
                        block,
                        log_index,
                        "a bind fee without the ceremony beside it"
                    ),
                }
            }

            NamesEvent::Deposited {
                handle_node,
                platform_id,
                ..
            }
            | NamesEvent::Forwarded {
                handle_node,
                platform_id,
                ..
            } => involvement.handle = Some((*handle_node, *platform_id)),

            // A claim ends the refunds of everybody whose deposit it took:
            // the round's refundable rows, read before the claim deletes them.
            NamesEvent::Claimed {
                handle_node,
                token,
                round,
                ..
            } => {
                let refund_tos: Vec<Vec<u8>> =
                    sqlx::query_scalar(sql::CLAIMED_REFUND_TOS)
                        .bind(chain_id)
                        .bind(handle_node.as_slice())
                        .bind(token.as_slice())
                        .bind(round.to_string())
                        .fetch_all(&mut *self.tx)
                        .await?;
                for refund_to in refund_tos {
                    if let Ok(refund_to) = Address::try_from(refund_to.as_slice()) {
                        involvement.add(refund_to, Role::RefundTo);
                    }
                }
                involvement.handle = self.escrow_handle(*handle_node, block).await?;
            }

            NamesEvent::Refunded { handle_node, .. } => {
                involvement.handle = self.escrow_handle(*handle_node, block).await?;
            }

            NamesEvent::PlatformConfigured { .. }
            | NamesEvent::ProofVerifierConfigured { .. } => {}
        }

        Ok(involvement)
    }

    /// An escrowed node with its platform, which only the slot's first deposit
    /// named: claims and refunds name none.
    async fn escrow_handle(
        &mut self,
        handle_node: B256,
        block: i64,
    ) -> Result<Option<(B256, B256)>, ApplyError> {
        let platform: Option<Vec<u8>> = sqlx::query_scalar(sql::ESCROW_PLATFORM)
            .bind(self.chain_id)
            .bind(handle_node.as_slice())
            .fetch_optional(&mut *self.tx)
            .await?;
        if platform.is_none() {
            tracing::warn!(
                %handle_node,
                block,
                "an escrow payout from a slot this index never saw deposited"
            );
        }
        Ok(platform.map(|platform| (handle_node, B256::from_slice(&platform))))
    }

    /// Put the event at `pos` in the histories of everything it involves.
    pub(super) async fn record(
        &mut self,
        involvement: Involvement,
        pos: &LogPosition,
    ) -> Result<(), ApplyError> {
        let chain_id = self.chain_id;
        let block = as_i64(pos.block_number, "block number")?;
        let log_index = as_i64(pos.log_index, "log index")?;
        let block_time = as_i64(pos.block_time, "block timestamp")?;
        for (address, roles) in involvement.parties {
            let roles: Vec<&str> = roles.into_iter().map(Role::as_str).collect();
            sqlx::query(sql::RECORD_ADDRESS_EVENT)
                .bind(chain_id)
                .bind(address.as_slice())
                .bind(block)
                .bind(log_index)
                .bind(block_time)
                .bind(roles)
                .execute(&mut *self.tx)
                .await?;
        }
        if let Some((handle_node, platform_id)) = involvement.handle {
            sqlx::query(sql::RECORD_HANDLE_EVENT)
                .bind(chain_id)
                .bind(block)
                .bind(log_index)
                .bind(handle_node.as_slice())
                .bind(platform_id.as_slice())
                .bind(block_time)
                .execute(&mut *self.tx)
                .await?;
        }
        Ok(())
    }
}

/// Where a history page ends: the last entry it served. A history is newest
/// first, ordered by block time, then chain, block and log index, so this is
/// the next page's exclusive upper bound. Its text is
/// `blockTime-chainId-blockNumber-logIndex`, which a client passes back
/// without reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCursor {
    /// The entry's block time, in unix seconds.
    pub block_time: i64,
    /// The entry's chain.
    pub chain_id: i64,
    /// The entry's block.
    pub block_number: i64,
    /// The entry's log index.
    pub log_index: i64,
}

/// Text that is not a cursor a history page handed out.
#[derive(Debug, thiserror::Error)]
#[error("not a history cursor")]
pub struct InvalidCursor;

impl FromStr for HistoryCursor {
    type Err = InvalidCursor;

    fn from_str(raw: &str) -> Result<Self, InvalidCursor> {
        let mut parts = raw.split('-').map(str::parse::<i64>);
        let mut next = || parts.next().and_then(Result::ok).ok_or(InvalidCursor);
        let cursor = Self {
            block_time: next()?,
            chain_id: next()?,
            block_number: next()?,
            log_index: next()?,
        };
        match parts.next() {
            None => Ok(cursor),
            Some(_) => Err(InvalidCursor),
        }
    }
}

impl std::fmt::Display for HistoryCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}-{}-{}-{}",
            self.block_time, self.chain_id, self.block_number, self.log_index
        )
    }
}

/// One history entry as the store holds it: the journal row, where it sat,
/// the roles an address history asked about, and the handle it concerns.
#[derive(sqlx::FromRow)]
pub struct HistoryRow {
    /// The chain the event happened on.
    pub chain_id: i64,
    /// Its block.
    pub block_number: i64,
    /// Its index in the block.
    pub log_index: i64,
    /// Its block's time, in unix seconds.
    pub block_time: i64,
    /// The parts the address asked about plays; `None` in a handle history.
    pub roles: Option<Vec<String>>,
    /// The transaction that emitted it.
    pub tx_hash: Vec<u8>,
    /// The journal's kind discriminator.
    pub kind: String,
    /// The journal payload, as [`NamesEvent::payload`] wrote it.
    pub payload: serde_json::Value,
    /// The handle node the event concerns, when it concerns one.
    pub handle_node: Option<Vec<u8>>,
    /// That node's platform.
    pub platform_id: Option<Vec<u8>>,
    /// That handle's text, when a bind ever carried it.
    pub handle: Option<String>,
}

/// One page of a history, as a request asks for it: on every chain or one,
/// older than a cursor or from the newest entry, at most `limit` entries.
#[derive(Debug, Clone, Copy)]
pub struct HistoryPage {
    /// One chain, or every chain the store holds.
    pub chain: Option<i64>,
    /// The cursor the previous page handed out, if this is not the first.
    pub before: Option<HistoryCursor>,
    /// The most entries the page holds.
    pub limit: i64,
}

/// A page as the store read it: its entries, and where the next page starts
/// while there is one.
pub struct HistoryRows {
    /// Newest first.
    pub rows: Vec<HistoryRow>,
    /// The cursor to pass back as the next page's `before`.
    pub next: Option<HistoryCursor>,
}

impl HistoryPage {
    /// An address's history page, composed per request like every lookup.
    pub(super) fn address_lookup(&self, prefix: &str, address: Address) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE a.address = ",
            sql::ADDRESS_HISTORY_PROJECTION
        ));
        statement.push_bind(address.as_slice().to_vec());
        scoped(&mut statement, "a.chain_id", self.chain);
        self.bound(&mut statement, "a");
        statement
    }

    /// A handle's history page.
    pub(super) fn node_lookup(&self, prefix: &str, handle_node: B256) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE he.handle_node = ",
            sql::HANDLE_HISTORY_PROJECTION
        ));
        statement.push_bind(handle_node.as_slice().to_vec());
        scoped(&mut statement, "he.chain_id", self.chain);
        self.bound(&mut statement, "he");
        statement
    }

    /// The cursor, the order and one row past the limit, which tells whether
    /// a next page exists. The cursor's row comparison and the order are the
    /// history index's column order, so a page is one backward range scan
    /// from the cursor.
    fn bound(&self, statement: &mut Statement, table: &str) {
        if let Some(before) = self.before {
            statement
                .push(format!(
                    " AND ({table}.block_time, {table}.chain_id, \
                     {table}.block_number, {table}.log_index) < ("
                ))
                .push_bind(before.block_time)
                .push(", ")
                .push_bind(before.chain_id)
                .push(", ")
                .push_bind(before.block_number)
                .push(", ")
                .push_bind(before.log_index)
                .push(")");
        }
        statement
            .push(format!(
                " ORDER BY {table}.block_time DESC, {table}.chain_id DESC, \
                 {table}.block_number DESC, {table}.log_index DESC LIMIT "
            ))
            .push_bind(self.limit + 1);
    }

    /// The page out of the rows its lookup read: at most `limit`, and the
    /// cursor after the last when the extra row proved more exist.
    fn split(&self, mut rows: Vec<HistoryRow>) -> HistoryRows {
        let limit = usize::try_from(self.limit).unwrap_or_default();
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = more
            .then(|| rows.last())
            .flatten()
            .map(|last| HistoryCursor {
                block_time: last.block_time,
                chain_id: last.chain_id,
                block_number: last.block_number,
                log_index: last.log_index,
            });
        HistoryRows { rows, next }
    }
}

impl Store {
    /// The events an address took part in, newest first.
    pub async fn address_history(
        &self,
        address: Address,
        page: HistoryPage,
    ) -> Result<HistoryRows, sqlx::Error> {
        let rows = page
            .address_lookup("", address)
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }

    /// The events on one handle node, newest first: deposits made while
    /// nobody held it, the binds that gave it a holder, and every claim and
    /// payment after.
    pub async fn handle_history(
        &self,
        handle_node: B256,
        page: HistoryPage,
    ) -> Result<HistoryRows, sqlx::Error> {
        let rows = page
            .node_lookup("", handle_node)
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::HistoryCursor;

    #[test]
    fn a_cursor_reads_back_what_it_wrote_and_nothing_else() {
        let cursor = HistoryCursor {
            block_time: 1_790_000_000,
            chain_id: 3_735_928_814,
            block_number: 274_217_956,
            log_index: 2,
        };
        assert_eq!(
            cursor.to_string().parse::<HistoryCursor>().ok(),
            Some(cursor)
        );
        for garbage in ["", "1-2-3", "1-2-3-4-5", "a-2-3-4", "1--2-3", "1-2-3-"] {
            assert!(garbage.parse::<HistoryCursor>().is_err(), "{garbage:?}");
        }
    }
}
