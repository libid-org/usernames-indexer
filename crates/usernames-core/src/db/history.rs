//! The histories: which addresses and which handle each journal row involves,
//! and the reads that list either newest first.
//!
//! What an event does not name is read from the rows it is about to change:
//! a bind's previous holders, the senders a claim takes from, and the handle
//! an unpublish withdraws. A ceremony and a fee take the handle and the payer
//! of the bind before them in their transaction.

use std::{
    fmt,
    str::FromStr,
};

use alloy::primitives::{
    Address,
    B256,
};
use sqlx::QueryBuilder;

use super::{
    journal::EventColumns,
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
use crate::{
    events::{
        EscrowEvent,
        EventKind,
        NamesEvent,
        Role,
    },
    nodes::DisclosedHandle,
};

/// What one event involves: the addresses, each with every part it plays in
/// it, and the handle node it concerns, with its platform when the event
/// names one.
pub(crate) struct Involvement {
    parties: Vec<(Address, Vec<Role>)>,
    pub(crate) handle_node: Option<B256>,
    platform_id: Option<B256>,
    /// The node's text, when the event disclosed it.
    handle: Option<DisclosedHandle>,
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

    /// The event names its handle's platform.
    fn named(&mut self, platform_id: B256) {
        self.platform_id = Some(platform_id);
    }
}

/// A bind as the events after it in its transaction need it: who bound
/// which handle node.
pub(crate) struct Bind {
    tx_hash: B256,
    holder: Address,
    handle_node: B256,
}

impl Window {
    /// What `event` involves, read before its projections move. A bind is
    /// remembered here for the ceremony and the fee after it.
    pub(crate) async fn involvement(
        &mut self,
        event: &NamesEvent,
        at: &Position,
    ) -> Result<Involvement, ApplyError> {
        let chain_id = self.chain_id;
        let mut involvement = Involvement {
            parties: Vec::new(),
            // The event's own node; the unpublish, the ceremony and the fee,
            // which name none, are given theirs below.
            handle_node: event.handle_node(),
            platform_id: None,
            handle: event.disclosure().map(|(_, _, text)| text.clone()),
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
                involvement.named(*platform_id);
                self.bind = Some(Bind {
                    tx_hash: at.tx_hash,
                    holder: *holder,
                    handle_node: *handle_node,
                });
            }

            // The name it withdraws, read before the delete. The contract
            // logs this only when it withdrew one; with no published row
            // here, the index missed it (a later START_BLOCK).
            NamesEvent::HandleUnpublished { holder } => {
                involvement.handle_node = sqlx::query_scalar(sql::PUBLISHED_NAME)
                    .bind(chain_id)
                    .bind(holder)
                    .fetch_optional(&mut *self.tx)
                    .await?;
            }

            // A ceremony and its fee follow their bind in its transaction:
            // they concern its handle, and the fee's payer is its holder.
            NamesEvent::CeremonyBound { .. } | NamesEvent::BindFeePaid { .. } => {
                match self.bind.as_ref().filter(|bind| bind.tx_hash == at.tx_hash) {
                    Some(bind) => {
                        involvement.add(bind.holder, Role::Holder);
                        involvement.handle_node = Some(bind.handle_node);
                    }
                    None => tracing::warn!(
                        block = at.block,
                        log_index = at.log_index,
                        kind = %event.kind(),
                        "a ceremony or a bind fee without the bind before it"
                    ),
                }
            }

            // These name the platform too: a reveal can come before any bind
            // names it, and the others matter when the index starts after
            // the node's bind.
            NamesEvent::HandleRevealed { platform_id, .. }
            | NamesEvent::HandlePublished { platform_id, .. }
            | NamesEvent::HandleRetired { platform_id, .. }
            | NamesEvent::Referred { platform_id, .. } => involvement.named(*platform_id),

            // A deposit names no platform; the `HandleRevealed` of a
            // `depositToHandle`, a bind, a publish or a retire does.
            NamesEvent::Escrow(
                EscrowEvent::Deposited { .. } | EscrowEvent::Forwarded { .. },
            ) => {}

            // A claim ends the refunds of everybody whose deposit it took:
            // the round's refundable rows, read before the claim deletes them.
            NamesEvent::Escrow(EscrowEvent::Claimed {
                handle_node, token, ..
            }) => {
                let senders: Vec<Address> = sqlx::query_scalar(sql::CLAIMED_SENDERS)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .fetch_all(&mut *self.tx)
                    .await?;
                for sender in senders {
                    involvement.add(sender, Role::Sender);
                }
            }

            NamesEvent::Escrow(EscrowEvent::Refunded { .. }) => {}

            NamesEvent::ProofVerifierConfigured { .. }
            | NamesEvent::BindFeeSet { .. } => {}
        }

        Ok(involvement)
    }

    /// Put the event at `at` in the history of every address it involves,
    /// and its handle node beside its platform and text.
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
        if let Some(platform_id) = involvement.platform_id {
            sqlx::query(sql::RECORD_HANDLE_NODE)
                .bind(chain_id)
                .bind(handle_node)
                .bind(platform_id)
                .bind(involvement.handle.as_ref().map(DisclosedHandle::as_str))
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

impl Cursor<HistoryRow> for HistoryCursor {
    fn after(row: &HistoryRow) -> Option<Self> {
        Some(Self {
            block_time: row.block_time,
            chain_id: row.chain_id,
            block_number: row.block_number,
            log_index: row.log_index,
        })
    }
}

/// A UTC day's length in seconds: a day is named by its first second.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// Which of a chain's events a chain history lists: one UTC day's or all of
/// them, of one kind or all, in one token or any. Each shape seeks an index
/// led by the chain, then the filter it names, then the block time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainEvents {
    /// The chain.
    pub chain_id: i64,
    /// The first second of a UTC day; its events only.
    pub day: Option<i64>,
    /// Events of this kind only.
    pub kind: Option<EventKind>,
    /// Events moving this token only.
    pub token: Option<Address>,
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
    /// The event, in the journal's columns.
    #[sqlx(flatten)]
    pub event: EventColumns,
    /// The platform of the handle it concerns.
    pub node_platform_id: Option<B256>,
    /// That handle's text, once the chain disclosed it.
    pub handle: Option<String>,
}

impl Page<HistoryCursor> {
    /// An address's history page.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
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
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    pub(crate) fn node_lookup(&self, prefix: &str, handle_node: B256) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE e.handle_node = ",
            sql::HANDLE_HISTORY_PROJECTION
        ));
        statement.push_bind(handle_node);
        self.bound(&mut statement, "e");
        statement
    }

    /// A chain's history page: the projection's rows on the chain, narrowed
    /// to the day, kind and token the filter names.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    pub(crate) fn chain_lookup(&self, prefix: &str, events: ChainEvents) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE e.chain_id = ",
            sql::HANDLE_HISTORY_PROJECTION
        ));
        statement.push_bind(events.chain_id);
        if let Some(day) = events.day {
            statement
                .push(" AND e.block_time >= ")
                .push_bind(day)
                .push(" AND e.block_time < ")
                .push_bind(day + SECONDS_PER_DAY);
        }
        if let Some(kind) = events.kind {
            statement.push(" AND e.kind = ").push_bind(kind.as_str());
        }
        if let Some(token) = events.token {
            statement.push(" AND e.token = ").push_bind(token);
        }
        self.on_one_chain(&mut statement, "e");
        statement
    }

    /// The chain, the cursor, the order and the fetch limit. On one chain the
    /// chain leaves the comparison, so the address table's key (chain,
    /// address, time) or the node's history index serves the page as one
    /// backward range; across chains the history index does, with the chain
    /// in the comparison.
    pub(crate) fn bound(&self, statement: &mut Statement, table: &str) {
        match self.chain {
            Some(chain) => {
                statement
                    .push(format!(" AND {table}.chain_id = "))
                    .push_bind(chain);
                self.on_one_chain(statement, table);
            }
            None => self.across_chains(statement, table),
        }
    }

    /// The cursor, the order and the fetch limit of a page across every
    /// chain: the chain joins the comparison and the order.
    fn across_chains(&self, statement: &mut Statement, table: &str) {
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
            .push_bind(self.fetch());
    }

    /// The cursor, the order and the fetch limit of a page whose statement
    /// already holds its one chain.
    fn on_one_chain(&self, statement: &mut Statement, table: &str) {
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
        statement
            .push(format!(
                " ORDER BY {table}.block_time DESC, {table}.block_number DESC, \
                 {table}.log_index DESC LIMIT "
            ))
            .push_bind(self.fetch());
    }
}

/// A page of a chain's events as the store read it.
pub struct ChainEventsPage {
    /// When it was read, in unix seconds by the database's clock.
    pub taken_at: i64,
    /// The events, and the cursor of the next page.
    pub rows: Rows<HistoryRow, HistoryCursor>,
}

impl Store {
    /// A chain's events, newest first, as `events` narrows them, and when
    /// they were read.
    pub async fn chain_history(
        &self,
        events: ChainEvents,
        page: Page<HistoryCursor>,
    ) -> Result<ChainEventsPage, sqlx::Error> {
        let mut tx = self.pool.begin_with(sql::STATS_SNAPSHOT).await?;
        let taken_at = sqlx::query_scalar(sql::STATS_TAKEN_AT)
            .fetch_one(&mut *tx)
            .await?;
        let rows = page
            .chain_lookup("", events)
            .build_query_as()
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ChainEventsPage {
            taken_at,
            rows: page.split(rows),
        })
    }

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
