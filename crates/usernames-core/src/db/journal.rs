//! The journal: every decoded event in typed columns, one row per log. A
//! row sets the columns its kind carries and leaves the rest NULL.

use std::str::FromStr;

use alloy::primitives::{
    Address,
    Bytes,
    B256,
    U256,
};

use sqlx::{
    Postgres,
    Transaction,
};

use super::{
    as_i64,
    sql,
    ApplyError,
    Position,
};
use crate::events::{
    EscrowEvent,
    EventKind,
    NamesEvent,
    UnknownKind,
};

/// A journal row that does not read back as the event its kind names.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// A kind this build does not write.
    #[error(transparent)]
    Kind(#[from] UnknownKind),
    /// A column the kind carries is NULL.
    #[error("{kind} row without {field}")]
    Missing {
        /// The row's kind.
        kind: String,
        /// The NULL column.
        field: &'static str,
    },
    /// A number column out of a `uint256`'s range, or a count out of its
    /// type's.
    #[error("{kind} row with {field} out of range")]
    Range {
        /// The row's kind.
        kind: String,
        /// The column.
        field: &'static str,
    },
}

/// One event's columns, as the journal stores them. `handle_node` is the
/// handle the event concerns, a ceremony's and a fee's included, which is
/// what a handle history lists; numbers travel as decimal text. The other
/// fields are the event parameters of the same names.
#[derive(Debug, Default, sqlx::FromRow)]
#[allow(missing_docs)]
pub struct EventColumns {
    /// The variant, as [`NamesEvent::kind`] names it.
    pub kind: String,
    /// The handle the event concerns.
    pub handle_node: Option<B256>,
    /// The platform the event names.
    pub platform_id: Option<B256>,
    pub holder: Option<Address>,
    pub id_node: Option<B256>,
    pub observed_at: Option<i64>,
    pub ceremony_version: Option<i32>,
    pub verifier: Option<Address>,
    pub authorization_digest: Option<B256>,
    pub client_identifier: Option<Vec<u8>>,
    pub receiver: Option<Address>,
    pub token: Option<Address>,
    pub sender: Option<Address>,
    pub claimer: Option<Address>,
    pub recipient: Option<Address>,
    pub round: Option<String>,
    pub amount: Option<String>,
    pub released: Option<String>,
    pub received: Option<String>,
    pub refundable_at: Option<i64>,
}

impl EventColumns {
    /// The columns `event` sets. An event that names a handle node is filed
    /// under it; `concerned` files the others: a ceremony and a fee under
    /// their bind's node, an unpublish under the name it withdrew.
    pub(crate) fn of(
        event: &NamesEvent,
        concerned: Option<B256>,
    ) -> Result<Self, ApplyError> {
        let text = |value: &U256| Some(value.to_string());
        let base = Self {
            kind: event.kind().to_string(),
            handle_node: event.handle_node().or(concerned),
            ..Self::default()
        };
        Ok(match event {
            NamesEvent::IdentityBound {
                holder,
                id_node,
                platform_id,
                observed_at,
                ceremony_version,
                ..
            } => Self {
                holder: Some(*holder),
                id_node: Some(*id_node),
                platform_id: Some(*platform_id),
                observed_at: Some(as_i64(*observed_at, "observedAt")?),
                ceremony_version: Some(i32::from(*ceremony_version)),
                ..base
            },
            NamesEvent::HandleRevealed { platform_id, .. } => Self {
                platform_id: Some(*platform_id),
                ..base
            },
            NamesEvent::HandlePublished {
                holder,
                platform_id,
                ..
            }
            | NamesEvent::HandleRetired {
                holder,
                platform_id,
                ..
            } => Self {
                holder: Some(*holder),
                platform_id: Some(*platform_id),
                ..base
            },
            NamesEvent::HandleUnpublished { holder } => Self {
                holder: Some(*holder),
                ..base
            },
            NamesEvent::ProofVerifierConfigured { verifier } => Self {
                verifier: Some(*verifier),
                ..base
            },
            NamesEvent::CeremonyBound {
                authorization_digest,
                holder,
                platform_id,
                client_identifier,
            } => Self {
                authorization_digest: Some(*authorization_digest),
                holder: Some(*holder),
                platform_id: Some(*platform_id),
                client_identifier: Some(client_identifier.to_vec()),
                ..base
            },
            NamesEvent::BindFeePaid {
                authorization_digest,
                receiver,
                amount,
            } => Self {
                authorization_digest: Some(*authorization_digest),
                receiver: Some(*receiver),
                amount: text(amount),
                ..base
            },
            NamesEvent::Escrow(EscrowEvent::Deposited {
                token,
                sender,
                round,
                amount,
                refundable_at,
                ..
            }) => Self {
                token: Some(*token),
                sender: Some(*sender),
                round: text(round),
                amount: text(amount),
                refundable_at: Some(as_i64(*refundable_at, "refundableAt")?),
                ..base
            },
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                token,
                sender,
                holder,
                amount,
                received,
                ..
            }) => Self {
                token: Some(*token),
                sender: Some(*sender),
                holder: Some(*holder),
                amount: text(amount),
                received: text(received),
                ..base
            },
            NamesEvent::Escrow(EscrowEvent::Claimed {
                token,
                claimer,
                recipient,
                round,
                released,
                received,
                ..
            }) => Self {
                token: Some(*token),
                claimer: Some(*claimer),
                recipient: Some(*recipient),
                round: text(round),
                released: text(released),
                received: text(received),
                ..base
            },
            NamesEvent::Escrow(EscrowEvent::Refunded {
                token,
                sender,
                recipient,
                round,
                released,
                received,
                ..
            }) => Self {
                token: Some(*token),
                sender: Some(*sender),
                recipient: Some(*recipient),
                round: text(round),
                released: text(released),
                received: text(received),
                ..base
            },
        })
    }

    /// Write the row at `at`; `false` when the journal already holds the
    /// log, which is what keeps a replayed window from applying twice.
    pub(crate) async fn journal(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        chain_id: i64,
        at: &Position,
    ) -> Result<bool, sqlx::Error> {
        let written = sqlx::query(sql::JOURNAL_EVENT)
            .bind(chain_id)
            .bind(at.block)
            .bind(at.log_index)
            .bind(at.tx_hash)
            .bind(at.block_time)
            .bind(&self.kind)
            .bind(self.handle_node)
            .bind(self.platform_id)
            .bind(self.holder)
            .bind(self.id_node)
            .bind(self.observed_at)
            .bind(self.ceremony_version)
            .bind(self.verifier)
            .bind(self.authorization_digest)
            .bind(&self.client_identifier)
            .bind(self.receiver)
            .bind(self.token)
            .bind(self.sender)
            .bind(self.claimer)
            .bind(self.recipient)
            .bind(&self.round)
            .bind(&self.amount)
            .bind(&self.released)
            .bind(&self.received)
            .bind(self.refundable_at)
            .execute(&mut **tx)
            .await?;
        Ok(written.rows_affected() == 1)
    }

    /// A column the kind carries.
    fn need<T>(&self, value: Option<T>, field: &'static str) -> Result<T, JournalError> {
        value.ok_or_else(|| JournalError::Missing {
            kind: self.kind.clone(),
            field,
        })
    }

    /// A `uint256` column the kind carries.
    fn number(
        &self,
        value: &Option<String>,
        field: &'static str,
    ) -> Result<U256, JournalError> {
        let text = self.need(value.as_deref(), field)?;
        U256::from_str(text).map_err(|_| self.range(field))
    }

    fn range(&self, field: &'static str) -> JournalError {
        JournalError::Range {
            kind: self.kind.clone(),
            field,
        }
    }
}

/// The event a journal row holds. Disclosed text never enters the journal,
/// so a `HandleRevealed` reads back without it.
impl TryFrom<&EventColumns> for NamesEvent {
    type Error = JournalError;

    fn try_from(row: &EventColumns) -> Result<Self, JournalError> {
        let handle_node = || row.need(row.handle_node, "handle_node");
        let platform_id = || row.need(row.platform_id, "platform_id");
        let holder = || row.need(row.holder, "holder");
        let token = || row.need(row.token, "token");
        let sender = || row.need(row.sender, "sender");
        let recipient = || row.need(row.recipient, "recipient");
        let unsigned = |value: Option<i64>, field| {
            let value = row.need(value, field)?;
            u64::try_from(value).map_err(|_| row.range(field))
        };
        Ok(match row.kind.parse::<EventKind>()? {
            EventKind::IdentityBound => Self::IdentityBound {
                holder: holder()?,
                id_node: row.need(row.id_node, "id_node")?,
                handle_node: handle_node()?,
                platform_id: platform_id()?,
                observed_at: unsigned(row.observed_at, "observed_at")?,
                ceremony_version: u16::try_from(
                    row.need(row.ceremony_version, "ceremony_version")?,
                )
                .map_err(|_| row.range("ceremony_version"))?,
            },
            EventKind::HandleRevealed => Self::HandleRevealed {
                platform_id: platform_id()?,
                handle_node: handle_node()?,
                handle: None,
            },
            EventKind::HandlePublished => Self::HandlePublished {
                holder: holder()?,
                platform_id: platform_id()?,
                handle_node: handle_node()?,
            },
            EventKind::HandleRetired => Self::HandleRetired {
                platform_id: platform_id()?,
                handle_node: handle_node()?,
                holder: holder()?,
            },
            EventKind::HandleUnpublished => Self::HandleUnpublished { holder: holder()? },
            EventKind::ProofVerifierConfigured => Self::ProofVerifierConfigured {
                verifier: row.need(row.verifier, "verifier")?,
            },
            EventKind::CeremonyBound => Self::CeremonyBound {
                authorization_digest: row
                    .need(row.authorization_digest, "authorization_digest")?,
                holder: holder()?,
                platform_id: platform_id()?,
                client_identifier: Bytes::copy_from_slice(
                    row.need(row.client_identifier.as_deref(), "client_identifier")?,
                ),
            },
            EventKind::BindFeePaid => Self::BindFeePaid {
                authorization_digest: row
                    .need(row.authorization_digest, "authorization_digest")?,
                receiver: row.need(row.receiver, "receiver")?,
                amount: row.number(&row.amount, "amount")?,
            },
            EventKind::Deposited => Self::Escrow(EscrowEvent::Deposited {
                handle_node: handle_node()?,
                token: token()?,
                sender: sender()?,
                round: row.number(&row.round, "round")?,
                amount: row.number(&row.amount, "amount")?,
                refundable_at: unsigned(row.refundable_at, "refundable_at")?,
            }),
            EventKind::Forwarded => Self::Escrow(EscrowEvent::Forwarded {
                handle_node: handle_node()?,
                token: token()?,
                sender: sender()?,
                holder: holder()?,
                amount: row.number(&row.amount, "amount")?,
                received: row.number(&row.received, "received")?,
            }),
            EventKind::Claimed => Self::Escrow(EscrowEvent::Claimed {
                handle_node: handle_node()?,
                token: token()?,
                claimer: row.need(row.claimer, "claimer")?,
                recipient: recipient()?,
                round: row.number(&row.round, "round")?,
                released: row.number(&row.released, "released")?,
                received: row.number(&row.received, "received")?,
            }),
            EventKind::Refunded => Self::Escrow(EscrowEvent::Refunded {
                handle_node: handle_node()?,
                token: token()?,
                sender: sender()?,
                recipient: recipient()?,
                round: row.number(&row.round, "round")?,
                released: row.number(&row.released, "released")?,
                received: row.number(&row.received, "received")?,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::{
        DisclosedHandle,
        KnownPlatform,
    };

    /// Every kind, the largest `uint256` and counts included, reads back from
    /// the columns it writes: all of it but disclosed text, which the journal
    /// never holds.
    #[test]
    fn every_event_reads_back_from_its_columns() {
        let (a, b) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));
        let (n, p) = (B256::repeat_byte(0x0D), B256::repeat_byte(0x0C));
        let big = U256::MAX - U256::from(1u64);
        let x = KnownPlatform::X;
        let alice = x.handle_node("alice").unwrap();
        let text = DisclosedHandle::verify(x, alice, "Alice");
        assert!(text.is_some());
        let events = [
            (
                NamesEvent::IdentityBound {
                    holder: a,
                    id_node: n,
                    handle_node: p,
                    platform_id: x.id(),
                    observed_at: i64::MAX as u64,
                    ceremony_version: u16::MAX,
                },
                Some(p),
            ),
            (
                NamesEvent::HandleRevealed {
                    platform_id: x.id(),
                    handle_node: alice,
                    handle: text,
                },
                Some(alice),
            ),
            (
                NamesEvent::HandlePublished {
                    holder: a,
                    platform_id: x.id(),
                    handle_node: alice,
                },
                Some(alice),
            ),
            (
                NamesEvent::HandleRetired {
                    platform_id: x.id(),
                    handle_node: n,
                    holder: a,
                },
                Some(n),
            ),
            (NamesEvent::HandleUnpublished { holder: a }, Some(alice)),
            (NamesEvent::ProofVerifierConfigured { verifier: b }, None),
            (
                NamesEvent::CeremonyBound {
                    authorization_digest: p,
                    holder: a,
                    platform_id: x.id(),
                    client_identifier: Bytes::from_static(b"client"),
                },
                Some(n),
            ),
            (
                NamesEvent::BindFeePaid {
                    authorization_digest: p,
                    receiver: b,
                    amount: big,
                },
                Some(n),
            ),
            (
                NamesEvent::Escrow(EscrowEvent::Deposited {
                    handle_node: n,
                    token: b,
                    sender: a,
                    round: U256::from(2u64),
                    amount: U256::MAX,
                    refundable_at: i64::MAX as u64,
                }),
                Some(n),
            ),
            (
                NamesEvent::Escrow(EscrowEvent::Forwarded {
                    handle_node: n,
                    token: b,
                    sender: a,
                    holder: b,
                    amount: big,
                    received: big,
                }),
                Some(n),
            ),
            (
                NamesEvent::Escrow(EscrowEvent::Claimed {
                    handle_node: n,
                    token: b,
                    claimer: a,
                    recipient: b,
                    round: U256::ZERO,
                    released: big,
                    received: U256::from(1u64),
                }),
                Some(n),
            ),
            (
                NamesEvent::Escrow(EscrowEvent::Refunded {
                    handle_node: n,
                    token: b,
                    sender: a,
                    recipient: b,
                    round: U256::from(1u64),
                    released: U256::from(5u64),
                    received: U256::from(4u64),
                }),
                Some(n),
            ),
        ];
        for (event, handle_node) in events {
            let row = EventColumns::of(&event, handle_node).unwrap();
            assert_eq!(row.handle_node, handle_node);
            let read = NamesEvent::try_from(&row)
                .unwrap_or_else(|e| panic!("{}: {e}", event.kind()));
            assert_eq!(read, event.without_text());
        }
    }

    /// A column the kind carries, left NULL, is refused by name.
    #[test]
    fn a_row_missing_a_column_is_refused_by_name() {
        let row = EventColumns {
            kind: "handle_unpublished".into(),
            ..EventColumns::default()
        };
        assert!(matches!(
            NamesEvent::try_from(&row),
            Err(JournalError::Missing {
                field: "holder",
                ..
            })
        ));
    }
}
