//! HandleEscrow's books: what each handle node holds per token, and what each
//! deposit's `refundTo` may still take back, moved the way the contract moves
//! its own storage — and the reads that answer "what is waiting" over them.

use alloy::primitives::{
    Address,
    B256,
    U256,
};
use sqlx::QueryBuilder;
use tracing::error;

use super::{
    scoped,
    sql,
    ApplyError,
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
    pub(super) async fn book_escrow(
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
                    .bind(handle_node.as_slice())
                    .bind(token.as_slice())
                    .bind(platform_id.as_slice())
                    .bind(amount.to_string())
                    .bind(round.to_string())
                    .bind(block)
                    .bind(log_index)
                    .execute(&mut *self.tx)
                    .await?;
                sqlx::query(sql::ESCROW_BOOK_REFUNDABLE)
                    .bind(chain_id)
                    .bind(handle_node.as_slice())
                    .bind(token.as_slice())
                    .bind(round.to_string())
                    .bind(refund_to.as_slice())
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
                    .bind(handle_node.as_slice())
                    .bind(token.as_slice())
                    .bind(round.to_string())
                    .bind(block)
                    .bind(log_index)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                if moved.is_none() {
                    error!(
                        %handle_node,
                        %token,
                        block,
                        "a claim on a slot this index never saw deposited"
                    );
                }
                sqlx::query(sql::ESCROW_CLOSE_ROUND)
                    .bind(chain_id)
                    .bind(handle_node.as_slice())
                    .bind(token.as_slice())
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
                    .bind(handle_node.as_slice())
                    .bind(token.as_slice())
                    .bind(released.to_string())
                    .bind(block)
                    .bind(log_index)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                if moved.is_none() {
                    error!(
                        %handle_node,
                        %token,
                        block,
                        "a refund from a slot this index never saw deposited"
                    );
                }
                sqlx::query(sql::ESCROW_TAKE_REFUNDABLE)
                    .bind(chain_id)
                    .bind(handle_node.as_slice())
                    .bind(token.as_slice())
                    .bind(round.to_string())
                    .bind(refund_to.as_slice())
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
    pub platform_id: Vec<u8>,
    /// The handle node the amount is held against.
    pub handle_node: Vec<u8>,
    /// The handle, when a bind ever carried its text. A deposit carries only
    /// the node, so a handle nobody has bound is known by its node alone.
    pub handle: Option<String>,
    /// Who holds the handle now and may claim: `None` while nobody does.
    pub holder: Option<Vec<u8>>,
    /// The token; EIP-7528 `0xEeee…EEeE` is the chain's own coin.
    pub token: Vec<u8>,
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

    /// The holder as an address, while somebody holds the handle.
    pub fn holder_address(&self) -> Option<Address> {
        self.holder
            .as_deref()
            .and_then(|bytes| <[u8; 20]>::try_from(bytes).ok())
            .map(Address::from)
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
        filter: UnclaimedFilter,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<EscrowSlotRow>, sqlx::Error> {
        unclaimed_lookup("", filter, limit, offset)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }
}

/// What narrows the unclaimed list. Every field is optional; none lists every
/// slot the store holds.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnclaimedFilter {
    /// One chain.
    pub chain: Option<i64>,
    /// One token.
    pub token: Option<Address>,
    /// One platform.
    pub platform_id: Option<B256>,
}

pub(super) fn claimable_lookup(
    prefix: &str,
    chain: Option<i64>,
    holder: Address,
) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} AND h.owner = ",
        sql::CLAIMABLE_PROJECTION
    ));
    statement.push_bind(holder.as_slice().to_vec());
    scoped(&mut statement, "h.chain_id", chain);
    statement.push(" ORDER BY e.chain_id, h.handle, e.token");
    statement
}

pub(super) fn refundable_lookup(
    prefix: &str,
    chain: Option<i64>,
    refund_to: Address,
) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} WHERE r.refund_to = ",
        sql::REFUNDABLE_PROJECTION
    ));
    statement.push_bind(refund_to.as_slice().to_vec());
    scoped(&mut statement, "r.chain_id", chain);
    statement.push(" ORDER BY r.chain_id, r.handle_node, r.token");
    statement
}

pub(super) fn node_slots_lookup(
    prefix: &str,
    chain: Option<i64>,
    handle_node: B256,
) -> Statement {
    let mut statement = QueryBuilder::new(format!(
        "{prefix}{} AND e.handle_node = ",
        sql::ESCROW_SLOT_PROJECTION
    ));
    statement.push_bind(handle_node.as_slice().to_vec());
    scoped(&mut statement, "e.chain_id", chain);
    statement.push(" ORDER BY e.chain_id, e.token");
    statement
}

/// Token first, so one asset's slots page together, and the largest amount
/// first within it; chain and node break ties so a page boundary is stable.
pub(super) fn unclaimed_lookup(
    prefix: &str,
    filter: UnclaimedFilter,
    limit: i64,
    offset: i64,
) -> Statement {
    let mut statement =
        QueryBuilder::new(format!("{prefix}{}", sql::ESCROW_SLOT_PROJECTION));
    if let Some(token) = filter.token {
        statement
            .push(" AND e.token = ")
            .push_bind(token.as_slice().to_vec());
    }
    scoped(&mut statement, "e.chain_id", filter.chain);
    if let Some(platform_id) = filter.platform_id {
        statement
            .push(" AND e.platform_id = ")
            .push_bind(platform_id.as_slice().to_vec());
    }
    statement
        .push(" ORDER BY e.token, e.held DESC, e.chain_id, e.handle_node LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    statement
}
