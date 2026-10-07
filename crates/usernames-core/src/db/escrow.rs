//! HandleEscrow's books: what each handle node holds per token, and what each
//! deposit's `refundTo` may still take back, moved the way the contract moves
//! its own storage — and the reads that answer "what is waiting" over them.

use std::{
    fmt,
    str::FromStr,
};

use alloy::primitives::{
    Address,
    B256,
    U256,
};
use sqlx::QueryBuilder;

use super::{
    scoped,
    sql,
    ApplyError,
    InvalidCursor,
    Statement,
    Store,
    Window,
};
use crate::events::NamesEvent;

impl Window {
    /// Move the escrow's books for one of its events. A claim or refund on a
    /// slot this index never saw deposited is reported, not refused: it means
    /// the scan started after the escrow's first deposit (`START_BLOCK`), and
    /// stalling the chain would not bring the deposit back.
    pub(crate) async fn book_escrow(
        &mut self,
        event: &NamesEvent,
        block: i64,
        log_index: i64,
    ) -> Result<(), ApplyError> {
        let chain_id = self.chain_id;
        match event {
            NamesEvent::Deposited {
                handle_node,
                token,
                refund_to,
                platform_id,
                round,
                amount,
                ..
            } => {
                sqlx::query(sql::ESCROW_DEPOSIT)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(platform_id)
                    .bind(amount.to_string())
                    .bind(round.to_string())
                    .bind(block)
                    .bind(log_index)
                    .execute(&mut *self.tx)
                    .await?;
                sqlx::query(sql::ESCROW_BOOK_REFUNDABLE)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(round.to_string())
                    .bind(refund_to)
                    .bind(amount.to_string())
                    .execute(&mut *self.tx)
                    .await?;
            }

            NamesEvent::Claimed {
                handle_node,
                token,
                round,
                ..
            } => {
                let moved: Option<Vec<u8>> = sqlx::query_scalar(sql::ESCROW_CLAIM)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(round.to_string())
                    .bind(block)
                    .bind(log_index)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                if moved.is_none() {
                    tracing::error!(
                        %handle_node,
                        %token,
                        block,
                        "a claim on a slot this index never saw deposited"
                    );
                }
                sqlx::query(sql::ESCROW_CLOSE_ROUND)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(round.to_string())
                    .execute(&mut *self.tx)
                    .await?;
            }

            NamesEvent::Refunded {
                handle_node,
                token,
                refund_to,
                round,
                released,
                ..
            } => {
                let moved: Option<Vec<u8>> = sqlx::query_scalar(sql::ESCROW_REFUND)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(released.to_string())
                    .bind(block)
                    .bind(log_index)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                if moved.is_none() {
                    tracing::error!(
                        %handle_node,
                        %token,
                        block,
                        "a refund from a slot this index never saw deposited"
                    );
                }
                sqlx::query(sql::ESCROW_TAKE_REFUNDABLE)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(round.to_string())
                    .bind(refund_to)
                    .execute(&mut *self.tx)
                    .await?;
            }

            // Paid straight to the holder: nothing is held, and the
            // histories are the whole record of it.
            NamesEvent::Forwarded { .. } => {}

            // Not the escrow's.
            NamesEvent::IdentityBound { .. }
            | NamesEvent::HandleRetired { .. }
            | NamesEvent::HandleUnpublished { .. }
            | NamesEvent::PlatformConfigured { .. }
            | NamesEvent::ProofVerifierConfigured { .. }
            | NamesEvent::CeremonyBound { .. }
            | NamesEvent::BindFeePaid { .. } => {}
        }
        Ok(())
    }
}

/// One amount waiting in the escrow, with the handle it waits for: a slot's
/// held amount, or one `refundTo`'s contribution to it.
#[derive(sqlx::FromRow)]
pub struct EscrowSlotRow {
    /// The chain the escrow holds it on.
    pub chain_id: i64,
    /// The platform of the handle, as the first deposit named it.
    pub platform_id: B256,
    /// The handle node the amount is held against.
    pub handle_node: B256,
    /// The handle, when a bind ever carried its text. A deposit carries only
    /// the node, so a handle nobody has bound is known by its node alone.
    pub handle: Option<String>,
    /// Who holds the handle now and may claim: `None` while nobody does.
    pub holder: Option<Address>,
    /// The token; EIP-7528 `0xEeee…EEeE` is the chain's own coin.
    pub token: Address,
    /// The amount in token units, as Postgres prints a `numeric`.
    pub amount: String,
    /// The slot's current round: claims so far.
    pub round: String,
}

impl EscrowSlotRow {
    /// The amount as the chain counts it. `None` only for a stored value that
    /// is not a `uint256`, which the books never write.
    pub fn amount(&self) -> Option<U256> {
        U256::from_str_radix(&self.amount, 10).ok()
    }

    /// The round as the chain counts it, under the same rule.
    pub fn round(&self) -> Option<U256> {
        U256::from_str_radix(&self.round, 10).ok()
    }
}

impl Store {
    /// What a holder may claim now: every slot holding something for a handle
    /// the address holds, on every chain or the one named.
    pub async fn claimable(
        &self,
        chain: Option<i64>,
        holder: Address,
    ) -> Result<Vec<EscrowSlotRow>, sqlx::Error> {
        claimable_lookup("", chain, holder)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// What an address may refund now: its contribution to every slot in the
    /// slot's current round, whether or not anybody holds the handle yet.
    pub async fn refundable(
        &self,
        chain: Option<i64>,
        refund_to: Address,
    ) -> Result<Vec<EscrowSlotRow>, sqlx::Error> {
        refundable_lookup("", chain, refund_to)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// What one handle node holds, token by token.
    pub async fn escrowed_for(
        &self,
        chain: Option<i64>,
        handle_node: B256,
    ) -> Result<Vec<EscrowSlotRow>, sqlx::Error> {
        node_slots_lookup("", chain, handle_node)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// Every slot still holding something: token by token, largest first.
    pub async fn unclaimed(
        &self,
        page: UnclaimedPage,
    ) -> Result<UnclaimedRows, sqlx::Error> {
        let rows = page
            .lookup("")
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }
}

/// Where an unclaimed page ends: the last slot it served. The list runs by
/// token, then amount, chain and node, all descending, so this is the next
/// page's exclusive upper bound. Its text is `token-amount-chainId-node`,
/// which a client passes back without reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnclaimedCursor {
    /// The slot's token.
    pub token: Address,
    /// What the slot held.
    pub held: U256,
    /// The slot's chain.
    pub chain_id: i64,
    /// The slot's handle node.
    pub handle_node: B256,
}

impl FromStr for UnclaimedCursor {
    type Err = InvalidCursor;

    fn from_str(raw: &str) -> Result<Self, InvalidCursor> {
        let mut parts = raw.split('-');
        let mut next = || parts.next().ok_or(InvalidCursor);
        let cursor = Self {
            token: next()?.parse().map_err(|_| InvalidCursor)?,
            held: next()?.parse().map_err(|_| InvalidCursor)?,
            chain_id: next()?.parse().map_err(|_| InvalidCursor)?,
            handle_node: next()?.parse().map_err(|_| InvalidCursor)?,
        };
        match parts.next() {
            None => Ok(cursor),
            Some(_) => Err(InvalidCursor),
        }
    }
}

impl fmt::Display for UnclaimedCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}-{}-{}",
            self.token, self.held, self.chain_id, self.handle_node
        )
    }
}

/// One page of the unclaimed list: narrowed by whatever it names, after the
/// slot a previous page ended with, at most `limit` slots.
#[derive(Debug, Clone, Copy)]
pub struct UnclaimedPage {
    /// One chain, or every chain the store holds.
    pub chain: Option<i64>,
    /// One token, or every token.
    pub token: Option<Address>,
    /// One platform, or every platform.
    pub platform_id: Option<B256>,
    /// The cursor the previous page handed out, if this is not the first.
    pub before: Option<UnclaimedCursor>,
    /// The most slots the page holds.
    pub limit: i64,
}

/// A page as the store read it: its slots, and where the next page starts
/// while there is one.
pub struct UnclaimedRows {
    /// Token by token, the largest amount first within each.
    pub rows: Vec<EscrowSlotRow>,
    /// The cursor to pass back as the next page's `before`.
    pub next: Option<UnclaimedCursor>,
}

impl UnclaimedPage {
    /// Every column of the order descends, so the cursor is one row
    /// comparison and a page is one backward scan of the unclaimed index; one
    /// row past the limit tells whether a next page exists.
    pub(crate) fn lookup(&self, prefix: &str) -> Statement {
        let mut statement =
            QueryBuilder::new(format!("{prefix}{}", sql::ESCROW_SLOT_PROJECTION));
        if let Some(token) = self.token {
            statement.push(" AND e.token = ").push_bind(token);
        }
        scoped(&mut statement, "e.chain_id", self.chain);
        if let Some(platform_id) = self.platform_id {
            statement
                .push(" AND e.platform_id = ")
                .push_bind(platform_id);
        }
        if let Some(before) = self.before {
            statement
                .push(" AND (e.token, e.held, e.chain_id, e.handle_node) < (")
                .push_bind(before.token)
                .push(", ")
                .push_bind(before.held.to_string())
                .push("::numeric, ")
                .push_bind(before.chain_id)
                .push(", ")
                .push_bind(before.handle_node)
                .push(")");
        }
        statement
            .push(
                " ORDER BY e.token DESC, e.held DESC, e.chain_id DESC, \
                 e.handle_node DESC LIMIT ",
            )
            .push_bind(self.limit + 1);
        statement
    }

    /// The page out of the rows its lookup read: at most `limit`, and the
    /// cursor after the last when the extra row proved more exist.
    fn split(&self, mut rows: Vec<EscrowSlotRow>) -> UnclaimedRows {
        let limit = usize::try_from(self.limit).unwrap_or_default();
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = more
            .then(|| rows.last())
            .flatten()
            .and_then(UnclaimedCursor::after);
        UnclaimedRows { rows, next }
    }
}

impl UnclaimedCursor {
    /// The cursor a page that ended with `row` hands out; `None` only for a
    /// stored value the books never write.
    fn after(row: &EscrowSlotRow) -> Option<Self> {
        Some(Self {
            token: row.token,
            held: row.amount()?,
            chain_id: row.chain_id,
            handle_node: row.handle_node,
        })
    }
}

pub(crate) fn claimable_lookup(
    prefix: &str,
    chain: Option<i64>,
    holder: Address,
) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} AND h.owner = ",
        sql::CLAIMABLE_PROJECTION
    ));
    statement.push_bind(holder);
    scoped(&mut statement, "h.chain_id", chain);
    statement.push(" ORDER BY e.chain_id, h.handle, e.token");
    statement
}

pub(crate) fn refundable_lookup(
    prefix: &str,
    chain: Option<i64>,
    refund_to: Address,
) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} WHERE r.refund_to = ",
        sql::REFUNDABLE_PROJECTION
    ));
    statement.push_bind(refund_to);
    scoped(&mut statement, "r.chain_id", chain);
    statement.push(" ORDER BY r.chain_id, r.handle_node, r.token");
    statement
}

pub(crate) fn node_slots_lookup(
    prefix: &str,
    chain: Option<i64>,
    handle_node: B256,
) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} AND e.handle_node = ",
        sql::ESCROW_SLOT_PROJECTION
    ));
    statement.push_bind(handle_node);
    scoped(&mut statement, "e.chain_id", chain);
    statement.push(" ORDER BY e.chain_id, e.token");
    statement
}
