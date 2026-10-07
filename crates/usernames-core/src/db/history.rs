//! The histories: which addresses and which handle each journal row involves,
//! written beside it, and the reads that list either newest first.
//!
//! What an event does not name is read from the rows it is about to change,
//! or from the rows its own transaction recorded just before it: a bind's
//! previous holders, a retirement's holder, the handle of a ceremony, a fee
//! or an unpublish, a fee's payer, and the refundTos a claim takes from.

use std::{
    fmt,
    str::FromStr,
};

use alloy::primitives::{
    Address,
    B256,
};
use serde_json::Value;
use sqlx::QueryBuilder;

use super::{
    page::{
        Cursor,
        CursorText,
        Page,
        Rows,
    },
    sql,
    ApplyError,
    InvalidCursor,
    Position,
    Statement,
    Store,
    Window,
};
use crate::events::{
    EscrowEvent,
    NamesEvent,
    Role,
};

/// What one event involves: the addresses, each with every part it plays in
/// it, and the handle node it concerns, with its platform when the event
/// names one.
pub(crate) struct Involvement {
    parties: Vec<(Address, Vec<Role>)>,
    handle_node: Option<B256>,
    platform_id: Option<B256>,
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

    /// The event concerns `handle_node`, of the platform it names.
    fn named(&mut self, handle_node: B256, platform_id: B256) {
        self.handle_node = Some(handle_node);
        self.platform_id = Some(platform_id);
    }
}

/// The ceremony a bind fee was paid beside: its journal row, and the handle
/// its bind recorded.
#[derive(sqlx::FromRow)]
struct FeeCeremony {
    kind: String,
    payload: Value,
    handle_node: Option<B256>,
}

impl Window {
    /// What `event` involves, read before its projections move.
    pub(crate) async fn involvement(
        &mut self,
        event: &NamesEvent,
        at: &Position,
    ) -> Result<Involvement, ApplyError> {
        let chain_id = self.chain_id;
        let mut involvement = Involvement {
            parties: Vec::new(),
            handle_node: None,
            platform_id: None,
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
                let (handle_holder, id_holder): (Option<Address>, Option<Address>) =
                    sqlx::query_as(sql::PREVIOUS_HOLDERS)
                        .bind(chain_id)
                        .bind(handle_node)
                        .bind(id_node)
                        .fetch_one(&mut *self.tx)
                        .await?;
                let previous = [
                    (handle_holder, Role::PreviousHandleHolder),
                    (id_holder, Role::PreviousIdHolder),
                ];
                for (previous, role) in previous {
                    if let Some(previous) = previous.filter(|p| p != holder) {
                        involvement.add(previous, role);
                    }
                }
                involvement.named(*handle_node, *platform_id);
            }

            // The event names the address whose bind retired the handle. When
            // the account moved to another address in that bind, the handle
            // was held by the one it left, and that is whose history it
            // belongs in.
            NamesEvent::HandleRetired {
                platform_id,
                handle_node,
                holder,
            } => {
                let held_by: Option<Option<Address>> =
                    sqlx::query_scalar(sql::HANDLE_HOLDER)
                        .bind(chain_id)
                        .bind(handle_node)
                        .fetch_optional(&mut *self.tx)
                        .await?;
                match held_by.flatten() {
                    Some(held_by) if held_by != *holder => {
                        involvement.add(held_by, Role::PreviousHandleHolder)
                    }
                    _ => involvement.add(*holder, Role::Holder),
                }
                involvement.named(*handle_node, *platform_id);
            }

            // Nothing published is still a withdrawal the contract accepts;
            // it concerns no handle.
            NamesEvent::HandleUnpublished {
                holder,
                platform_id,
            } => {
                involvement.handle_node = sqlx::query_scalar(sql::PUBLISHED_HANDLE_NODE)
                    .bind(chain_id)
                    .bind(holder)
                    .bind(platform_id)
                    .fetch_optional(&mut *self.tx)
                    .await?;
            }

            NamesEvent::CeremonyBound {
                holder,
                platform_id,
                ..
            } => {
                involvement.handle_node = sqlx::query_scalar(sql::CEREMONY_HANDLE)
                    .bind(chain_id)
                    .bind(at.block)
                    .bind(at.tx_hash)
                    .bind(at.log_index)
                    .bind(holder)
                    .bind(at.block_time)
                    .bind(Role::Holder.as_str())
                    .bind(platform_id)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                if involvement.handle_node.is_none() {
                    tracing::warn!(
                        block = at.block,
                        log_index = at.log_index,
                        "a ceremony without the bind beside it"
                    );
                }
            }

            NamesEvent::BindFeePaid {
                authorization_digest,
                ..
            } => {
                let ceremony: Option<FeeCeremony> = sqlx::query_as(sql::FEE_PAYER)
                    .bind(chain_id)
                    .bind(at.block)
                    .bind(at.tx_hash)
                    .bind(authorization_digest.to_string())
                    .fetch_optional(&mut *self.tx)
                    .await?;
                let Some(ceremony) = ceremony else {
                    tracing::warn!(
                        block = at.block,
                        log_index = at.log_index,
                        "a bind fee without the ceremony beside it"
                    );
                    return Ok(involvement);
                };
                if let NamesEvent::CeremonyBound { holder, .. } =
                    NamesEvent::from_journal(&ceremony.kind, ceremony.payload)?
                {
                    involvement.add(holder, Role::Holder);
                }
                involvement.handle_node = ceremony.handle_node;
            }

            NamesEvent::Escrow(
                EscrowEvent::Deposited {
                    handle_node,
                    platform_id,
                    ..
                }
                | EscrowEvent::Forwarded {
                    handle_node,
                    platform_id,
                    ..
                },
            ) => involvement.named(*handle_node, *platform_id),

            // A claim ends the refunds of everybody whose deposit it took:
            // the round's refundable rows, read before the claim deletes them.
            NamesEvent::Escrow(EscrowEvent::Claimed {
                handle_node,
                token,
                round,
                ..
            }) => {
                let refund_tos: Vec<Address> =
                    sqlx::query_scalar(sql::CLAIMED_REFUND_TOS)
                        .bind(chain_id)
                        .bind(handle_node)
                        .bind(token)
                        .bind(round.to_string())
                        .fetch_all(&mut *self.tx)
                        .await?;
                for refund_to in refund_tos {
                    involvement.add(refund_to, Role::RefundTo);
                }
                involvement.handle_node = Some(*handle_node);
            }

            NamesEvent::Escrow(EscrowEvent::Refunded { handle_node, .. }) => {
                involvement.handle_node = Some(*handle_node);
            }

            NamesEvent::PlatformConfigured { .. }
            | NamesEvent::ProofVerifierConfigured { .. } => {}
        }

        Ok(involvement)
    }

    /// Put the event at `at` in the histories of everything it involves.
    pub(crate) async fn record(
        &mut self,
        involvement: Involvement,
        at: &Position,
    ) -> Result<(), ApplyError> {
        let chain_id = self.chain_id;
        for (address, roles) in involvement.parties {
            let roles: Vec<&str> = roles.into_iter().map(Role::as_str).collect();
            sqlx::query(sql::RECORD_ADDRESS_EVENT)
                .bind(chain_id)
                .bind(address)
                .bind(at.block_time)
                .bind(at.block)
                .bind(at.log_index)
                .bind(roles)
                .execute(&mut *self.tx)
                .await?;
        }
        let Some(handle_node) = involvement.handle_node else {
            return Ok(());
        };
        sqlx::query(sql::RECORD_HANDLE_EVENT)
            .bind(chain_id)
            .bind(at.block)
            .bind(at.log_index)
            .bind(handle_node)
            .bind(at.block_time)
            .execute(&mut *self.tx)
            .await?;
        if let Some(platform_id) = involvement.platform_id {
            sqlx::query(sql::RECORD_HANDLE_NODE)
                .bind(chain_id)
                .bind(handle_node)
                .bind(platform_id)
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

impl FromStr for HistoryCursor {
    type Err = InvalidCursor;

    fn from_str(raw: &str) -> Result<Self, InvalidCursor> {
        let mut parts = CursorText::of(raw);
        let cursor = Self {
            block_time: parts.next()?,
            chain_id: parts.next()?,
            block_number: parts.next()?,
            log_index: parts.next()?,
        };
        parts.end(cursor)
    }
}

impl fmt::Display for HistoryCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}-{}-{}",
            self.block_time, self.chain_id, self.block_number, self.log_index
        )
    }
}

impl Cursor for HistoryCursor {
    type Row = HistoryRow;

    fn after(row: &HistoryRow) -> Option<Self> {
        Some(Self {
            block_time: row.block_time,
            chain_id: row.chain_id,
            block_number: row.block_number,
            log_index: row.log_index,
        })
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
    pub tx_hash: B256,
    /// The journal's kind discriminator.
    pub kind: String,
    /// The journal payload, as [`NamesEvent::payload`] wrote it.
    pub payload: Value,
    /// The handle node the event concerns, when it concerns one.
    pub handle_node: Option<B256>,
    /// That node's platform.
    pub platform_id: Option<B256>,
    /// That handle's text, when a bind ever carried it.
    pub handle: Option<String>,
}

impl Page<HistoryCursor> {
    /// An address's history page.
    pub(crate) fn address_lookup(&self, prefix: &str, address: Address) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE a.address = ",
            sql::ADDRESS_HISTORY_PROJECTION
        ));
        statement.push_bind(address);
        self.bound(&mut statement, "a");
        statement
    }

    /// A handle's history page.
    pub(crate) fn node_lookup(&self, prefix: &str, handle_node: B256) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE he.handle_node = ",
            sql::HANDLE_HISTORY_PROJECTION
        ));
        statement.push_bind(handle_node);
        self.bound(&mut statement, "he");
        statement
    }

    /// The chain, the cursor, the order and the fetch limit. On one chain the
    /// chain leaves the comparison, so the table's key — chain, then time —
    /// serves the page as one backward range; across chains the history
    /// index does, with the chain in the comparison.
    fn bound(&self, statement: &mut Statement, table: &str) {
        let order = match self.chain {
            Some(chain) => {
                statement
                    .push(format!(" AND {table}.chain_id = "))
                    .push_bind(chain);
                if let Some(before) = self.before {
                    statement
                        .push(format!(
                            " AND ({table}.block_time, {table}.block_number, \
                             {table}.log_index) < ("
                        ))
                        .push_bind(before.block_time)
                        .push(", ")
                        .push_bind(before.block_number)
                        .push(", ")
                        .push_bind(before.log_index)
                        .push(")");
                }
                format!(
                    " ORDER BY {table}.block_time DESC, {table}.block_number DESC, \
                     {table}.log_index DESC LIMIT "
                )
            }
            None => {
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
                format!(
                    " ORDER BY {table}.block_time DESC, {table}.chain_id DESC, \
                     {table}.block_number DESC, {table}.log_index DESC LIMIT "
                )
            }
        };
        statement.push(order).push_bind(self.fetch());
    }
}

impl Store {
    /// The events an address took part in, newest first.
    pub async fn address_history(
        &self,
        address: Address,
        page: Page<HistoryCursor>,
    ) -> Result<Rows<HistoryRow, HistoryCursor>, sqlx::Error> {
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
        page: Page<HistoryCursor>,
    ) -> Result<Rows<HistoryRow, HistoryCursor>, sqlx::Error> {
        let rows = page
            .node_lookup("", handle_node)
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
    use crate::db::plan_tests::{
        explain,
        Plan,
    };

    /// The histories' lookups and the indexer's reads, in every shape they are
    /// asked in.
    pub(crate) async fn all(conn: &mut PgConnection) -> Vec<Plan> {
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
        let handle: &[&str] = &["handle_events_history_idx"];
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
            plans.push(Plan::of(conn, name, Some(seeks), statement).await);
        }

        let reads = [
            (
                "previous holders",
                &["handles_pkey", "ids_pkey"][..],
                sqlx::query_scalar::<_, String>(&explain(sql::PREVIOUS_HOLDERS))
                    .bind(1i64)
                    .bind(node)
                    .bind(node)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "a retired handle's holder",
                &["handles_pkey"][..],
                sqlx::query_scalar(&explain(sql::HANDLE_HOLDER))
                    .bind(1i64)
                    .bind(node)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "an unpublished handle",
                &["published_pkey", "handles_platform_handle_chain_idx"][..],
                sqlx::query_scalar(&explain(sql::PUBLISHED_HANDLE_NODE))
                    .bind(1i64)
                    .bind(address)
                    .bind(node)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "a ceremony's handle",
                &[
                    "handle_events_pkey",
                    "events_pkey",
                    "handle_nodes_pkey",
                    "address_events_pkey|address_events_history_idx",
                ][..],
                sqlx::query_scalar(&explain(sql::CEREMONY_HANDLE))
                    .bind(1i64)
                    .bind(10i64)
                    .bind(node)
                    .bind(3i64)
                    .bind(address)
                    .bind(1_700_000_000i64)
                    .bind(Role::Holder.as_str())
                    .bind(node)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "a bind fee's ceremony",
                &["events_pkey"][..],
                sqlx::query_scalar(&explain(sql::FEE_PAYER))
                    .bind(1i64)
                    .bind(10i64)
                    .bind(node)
                    .bind(node.to_string())
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "the refundTos a claim takes from",
                &["escrow_refundable_round_idx"][..],
                sqlx::query_scalar(&explain(sql::CLAIMED_REFUND_TOS))
                    .bind(1i64)
                    .bind(node)
                    .bind(address)
                    .bind("0")
                    .fetch_all(&mut *conn)
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
