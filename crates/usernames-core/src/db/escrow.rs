//! HandleEscrow's books: what each handle node holds per token, and what each
//! deposit's `refundTo` may still take back, moved the way the contract moves
//! its own storage — and the lists of what is waiting, paged by their keys.

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
use crate::events::EscrowEvent;

impl Window {
    /// Move the escrow's books for one of its events. A claim or refund on a
    /// slot this index never saw deposited is reported, not refused: it means
    /// the scan started after the escrow's first deposit (`START_BLOCK`), and
    /// stalling the chain would not bring the deposit back.
    pub(crate) async fn book_escrow(
        &mut self,
        event: &EscrowEvent,
        at: &Position,
    ) -> Result<(), ApplyError> {
        let chain_id = self.chain_id;
        match event {
            EscrowEvent::Deposited {
                handle_node,
                token,
                refund_to,
                round,
                amount,
                ..
            } => {
                sqlx::query(sql::ESCROW_DEPOSIT)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(amount.to_string())
                    .bind(round.to_string())
                    .bind(at.block)
                    .bind(at.log_index)
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

            EscrowEvent::Claimed {
                handle_node,
                token,
                round,
                ..
            } => {
                let moved: Option<B256> = sqlx::query_scalar(sql::ESCROW_CLAIM)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(round.to_string())
                    .bind(at.block)
                    .bind(at.log_index)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                if moved.is_none() {
                    tracing::error!(
                        %handle_node,
                        %token,
                        block = at.block,
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

            EscrowEvent::Refunded {
                handle_node,
                token,
                refund_to,
                round,
                released,
                ..
            } => {
                let moved: Option<B256> = sqlx::query_scalar(sql::ESCROW_REFUND)
                    .bind(chain_id)
                    .bind(handle_node)
                    .bind(token)
                    .bind(released.to_string())
                    .bind(at.block)
                    .bind(at.log_index)
                    .fetch_optional(&mut *self.tx)
                    .await?;
                if moved.is_none() {
                    tracing::error!(
                        %handle_node,
                        %token,
                        block = at.block,
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
            EscrowEvent::Forwarded { .. } => {}
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
    /// The platform of the handle.
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
    /// The slot's current round, or the round a contribution was booked in.
    pub round: String,
}

impl EscrowSlotRow {
    /// The amount as the chain counts it. `None` only for a stored value that
    /// is not a `uint256`, which the books never write.
    pub fn amount(&self) -> Option<U256> {
        self.amount.parse().ok()
    }

    /// The round as the chain counts it, under the same rule.
    pub fn round(&self) -> Option<U256> {
        self.round.parse().ok()
    }
}

/// Where an unclaimed page ends. The list runs by token, then amount, chain
/// and node, all descending, so this is the next page's exclusive upper
/// bound. Its text is `token-amount-chainId-node`.
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
        let mut parts = CursorText::of(raw);
        let cursor = Self {
            token: parts.next()?,
            held: parts.next()?,
            chain_id: parts.next()?,
            handle_node: parts.next()?,
        };
        parts.end(cursor)
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

impl Cursor for UnclaimedCursor {
    type Row = EscrowSlotRow;

    fn after(row: &EscrowSlotRow) -> Option<Self> {
        Some(Self {
            token: row.token,
            held: row.amount()?,
            chain_id: row.chain_id,
            handle_node: row.handle_node,
        })
    }
}

/// Where a page of slots ends: a holder's claimable slots, or one handle's.
/// Both run by chain, node and token, descending. Its text is
/// `chainId-node-token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotCursor {
    /// The slot's chain.
    pub chain_id: i64,
    /// The slot's handle node.
    pub handle_node: B256,
    /// The slot's token.
    pub token: Address,
}

impl FromStr for SlotCursor {
    type Err = InvalidCursor;

    fn from_str(raw: &str) -> Result<Self, InvalidCursor> {
        let mut parts = CursorText::of(raw);
        let cursor = Self {
            chain_id: parts.next()?,
            handle_node: parts.next()?,
            token: parts.next()?,
        };
        parts.end(cursor)
    }
}

impl fmt::Display for SlotCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}-{}", self.chain_id, self.handle_node, self.token)
    }
}

impl Cursor for SlotCursor {
    type Row = EscrowSlotRow;

    fn after(row: &EscrowSlotRow) -> Option<Self> {
        Some(Self {
            chain_id: row.chain_id,
            handle_node: row.handle_node,
            token: row.token,
        })
    }
}

/// Where a page of an address's refundable contributions ends: by chain,
/// node, token and round, descending. Its text is
/// `chainId-node-token-round`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefundCursor {
    /// The contribution's chain.
    pub chain_id: i64,
    /// The node it was deposited for.
    pub handle_node: B256,
    /// Its token.
    pub token: Address,
    /// The round it was booked in.
    pub round: U256,
}

impl FromStr for RefundCursor {
    type Err = InvalidCursor;

    fn from_str(raw: &str) -> Result<Self, InvalidCursor> {
        let mut parts = CursorText::of(raw);
        let cursor = Self {
            chain_id: parts.next()?,
            handle_node: parts.next()?,
            token: parts.next()?,
            round: parts.next()?,
        };
        parts.end(cursor)
    }
}

impl fmt::Display for RefundCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}-{}-{}",
            self.chain_id, self.handle_node, self.token, self.round
        )
    }
}

impl Cursor for RefundCursor {
    type Row = EscrowSlotRow;

    fn after(row: &EscrowSlotRow) -> Option<Self> {
        Some(Self {
            chain_id: row.chain_id,
            handle_node: row.handle_node,
            token: row.token,
            round: row.round()?,
        })
    }
}

impl Page<UnclaimedCursor> {
    /// Every slot still holding something, or every one in `token`. Across
    /// chains the unclaimed index serves the page; on one chain, its chain
    /// twin, with the chain out of the comparison.
    pub(crate) fn unclaimed_lookup(
        &self,
        prefix: &str,
        token: Option<Address>,
    ) -> Statement {
        let mut statement =
            QueryBuilder::new(format!("{prefix}{}", sql::ESCROW_SLOT_PROJECTION));
        if let Some(token) = token {
            statement.push(" AND e.token = ").push_bind(token);
        }
        let order = match self.chain {
            Some(chain) => {
                statement.push(" AND e.chain_id = ").push_bind(chain);
                if let Some(before) = self.before {
                    statement
                        .push(" AND (e.token, e.held, e.handle_node) < (")
                        .push_bind(before.token)
                        .push(", ")
                        .push_bind(before.held.to_string())
                        .push("::numeric, ")
                        .push_bind(before.handle_node)
                        .push(")");
                }
                " ORDER BY e.token DESC, e.held DESC, e.handle_node DESC LIMIT "
            }
            None => {
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
                " ORDER BY e.token DESC, e.held DESC, e.chain_id DESC, \
                 e.handle_node DESC LIMIT "
            }
        };
        statement.push(order).push_bind(self.fetch());
        statement
    }
}

impl Page<SlotCursor> {
    /// What `holder` may claim: the slots of the handles it holds, walked in
    /// the order of its handles and their tokens.
    pub(crate) fn claimable_lookup(&self, prefix: &str, holder: Address) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} AND h.owner = ",
            sql::CLAIMABLE_PROJECTION
        ));
        statement.push_bind(holder);
        match self.chain {
            Some(chain) => {
                statement.push(" AND h.chain_id = ").push_bind(chain);
                if let Some(before) = self.before {
                    statement
                        .push(" AND h.handle_node <= ")
                        .push_bind(before.handle_node)
                        .push(" AND (e.handle_node, e.token) < (")
                        .push_bind(before.handle_node)
                        .push(", ")
                        .push_bind(before.token)
                        .push(")");
                }
            }
            None => {
                if let Some(before) = self.before {
                    statement
                        .push(" AND (h.chain_id, h.handle_node) <= (")
                        .push_bind(before.chain_id)
                        .push(", ")
                        .push_bind(before.handle_node)
                        .push(") AND (e.chain_id, e.handle_node, e.token) < (")
                        .push_bind(before.chain_id)
                        .push(", ")
                        .push_bind(before.handle_node)
                        .push(", ")
                        .push_bind(before.token)
                        .push(")");
                }
            }
        }
        statement
            .push(" ORDER BY e.chain_id DESC, e.handle_node DESC, e.token DESC LIMIT ")
            .push_bind(self.fetch());
        statement
    }

    /// One handle node's slots, token by token.
    pub(crate) fn slots_lookup(&self, prefix: &str, handle_node: B256) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} AND e.handle_node = ",
            sql::ESCROW_SLOT_PROJECTION
        ));
        statement.push_bind(handle_node);
        match self.chain {
            Some(chain) => {
                statement.push(" AND e.chain_id = ").push_bind(chain);
                if let Some(before) = self.before {
                    statement.push(" AND e.token < ").push_bind(before.token);
                }
            }
            None => {
                if let Some(before) = self.before {
                    statement
                        .push(" AND (e.handle_node, e.chain_id, e.token) < (")
                        .push_bind(handle_node)
                        .push(", ")
                        .push_bind(before.chain_id)
                        .push(", ")
                        .push_bind(before.token)
                        .push(")");
                }
            }
        }
        // The node leads the comparison and the order as it leads the node
        // index, the one index whose order serves the page.
        statement
            .push(" ORDER BY e.handle_node DESC, e.chain_id DESC, e.token DESC LIMIT ")
            .push_bind(self.fetch());
        statement
    }
}

impl Page<RefundCursor> {
    /// What `refund_to` may take back: its contributions in their slots'
    /// current rounds, in the order of the table's key.
    pub(crate) fn refundable_lookup(
        &self,
        prefix: &str,
        refund_to: Address,
    ) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE r.refund_to = ",
            sql::REFUNDABLE_PROJECTION
        ));
        statement.push_bind(refund_to);
        match self.chain {
            Some(chain) => {
                statement.push(" AND r.chain_id = ").push_bind(chain);
                if let Some(before) = self.before {
                    statement
                        .push(" AND (r.handle_node, r.token, r.round) < (")
                        .push_bind(before.handle_node)
                        .push(", ")
                        .push_bind(before.token)
                        .push(", ")
                        .push_bind(before.round.to_string())
                        .push("::numeric)");
                }
            }
            None => {
                if let Some(before) = self.before {
                    statement
                        .push(" AND (r.chain_id, r.handle_node, r.token, r.round) < (")
                        .push_bind(before.chain_id)
                        .push(", ")
                        .push_bind(before.handle_node)
                        .push(", ")
                        .push_bind(before.token)
                        .push(", ")
                        .push_bind(before.round.to_string())
                        .push("::numeric)");
                }
            }
        }
        statement
            .push(
                " ORDER BY r.chain_id DESC, r.handle_node DESC, r.token DESC, \
                 r.round DESC LIMIT ",
            )
            .push_bind(self.fetch());
        statement
    }
}

impl Store {
    /// What a holder may claim now: every slot holding something for a handle
    /// the address holds.
    pub async fn claimable(
        &self,
        holder: Address,
        page: Page<SlotCursor>,
    ) -> Result<Rows<EscrowSlotRow, SlotCursor>, sqlx::Error> {
        let rows = page
            .claimable_lookup("", holder)
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }

    /// What an address may refund now: its contribution to every slot in the
    /// slot's current round, whether or not anybody holds the handle yet.
    pub async fn refundable(
        &self,
        refund_to: Address,
        page: Page<RefundCursor>,
    ) -> Result<Rows<EscrowSlotRow, RefundCursor>, sqlx::Error> {
        let rows = page
            .refundable_lookup("", refund_to)
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }

    /// What one handle node holds, token by token.
    pub async fn escrowed_for(
        &self,
        handle_node: B256,
        page: Page<SlotCursor>,
    ) -> Result<Rows<EscrowSlotRow, SlotCursor>, sqlx::Error> {
        let rows = page
            .slots_lookup("", handle_node)
            .build_query_as()
            .fetch_all(&self.pool)
            .await?;
        Ok(page.split(rows))
    }

    /// Every slot still holding something, or every one in `token`: token by
    /// token, largest first.
    pub async fn unclaimed(
        &self,
        token: Option<Address>,
        page: Page<UnclaimedCursor>,
    ) -> Result<Rows<EscrowSlotRow, UnclaimedCursor>, sqlx::Error> {
        let rows = page
            .unclaimed_lookup("", token)
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

    fn page<C>(chain: Option<i64>, before: Option<C>) -> Page<C> {
        Page {
            chain,
            before,
            limit: 20,
        }
    }

    /// The escrow's lists and the books' writes, in every shape they are asked in.
    pub(crate) async fn all(conn: &mut PgConnection) -> Vec<Plan> {
        let (address, node) = (Address::repeat_byte(2), B256::repeat_byte(1));
        // A cursor high in the token range, where a list has most of its
        // rows still ahead of it.
        let token = Address::repeat_byte(0xEE);
        let slot = Some(SlotCursor {
            chain_id: 1,
            handle_node: node,
            token,
        });
        let refund = Some(RefundCursor {
            chain_id: 1,
            handle_node: node,
            token,
            round: U256::ZERO,
        });
        let unclaimed = Some(UnclaimedCursor {
            token,
            held: U256::from(10u64),
            chain_id: 1,
            handle_node: node,
        });
        let claimable: &[&str] = &[
            "handles_owner_chain_node_idx",
            "escrow_held_node_idx|escrow_held_pkey",
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
                page(None, refund).refundable_lookup("EXPLAIN ", address),
            ),
            (
                "refundable, one chain, past a cursor",
                refundable,
                page(Some(1), refund).refundable_lookup("EXPLAIN ", address),
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
            plans.push(Plan::of(conn, name, Some(seeks), statement).await);
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
                    .bind(10i64)
                    .bind(0i64)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "a claim closing its round",
                &["escrow_refundable_round_idx"][..],
                sqlx::query_scalar(&explain(sql::ESCROW_CLOSE_ROUND))
                    .bind(1i64)
                    .bind(node)
                    .bind(token)
                    .bind("0")
                    .fetch_all(&mut *conn)
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
                    .bind(10i64)
                    .bind(0i64)
                    .fetch_all(&mut *conn)
                    .await,
            ),
            (
                "a refund taking its contribution",
                &["escrow_refundable_pkey|escrow_refundable_round_idx"][..],
                sqlx::query_scalar(&explain(sql::ESCROW_TAKE_REFUNDABLE))
                    .bind(1i64)
                    .bind(node)
                    .bind(token)
                    .bind("0")
                    .bind(address)
                    .fetch_all(&mut *conn)
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

#[cfg(test)]
mod tests {
    use alloy::primitives::{
        Address,
        B256,
        U256,
    };

    use super::{
        RefundCursor,
        SlotCursor,
        UnclaimedCursor,
    };

    #[test]
    fn every_cursor_reads_back_what_it_wrote_and_nothing_else() {
        let (token, node) = (Address::repeat_byte(0xEE), B256::repeat_byte(0x0D));
        let unclaimed = UnclaimedCursor {
            token,
            held: U256::MAX,
            chain_id: 3_735_928_814,
            handle_node: node,
        };
        assert_eq!(unclaimed.to_string().parse().ok(), Some(unclaimed));
        let slot = SlotCursor {
            chain_id: 1,
            handle_node: node,
            token,
        };
        assert_eq!(slot.to_string().parse().ok(), Some(slot));
        let refund = RefundCursor {
            chain_id: 1,
            handle_node: node,
            token,
            round: U256::from(7u64),
        };
        assert_eq!(refund.to_string().parse().ok(), Some(refund));

        let slot = slot.to_string();
        for garbage in ["", "1-0x0d", &format!("{slot}-1"), &format!("x{slot}")] {
            assert!(garbage.parse::<SlotCursor>().is_err(), "{garbage:?}");
        }
        assert!("1-2-3-4".parse::<UnclaimedCursor>().is_err());
        assert!(format!("1-{node}-{token}-x")
            .parse::<RefundCursor>()
            .is_err());
    }
}
