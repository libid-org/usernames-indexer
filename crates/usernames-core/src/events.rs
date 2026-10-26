//! Decoding `IdentityRegistry` and `HandleEscrow` logs into one typed stream.
//!
//! The enum exists so the apply path and its tests speak decoded values, not
//! `alloy` logs: a test builds a `NamesEvent` directly and never touches RPC
//! types.

use std::{
    fmt,
    str::FromStr,
};

use alloy::{
    primitives::{
        Address,
        Bytes,
        B256,
        U256,
    },
    rpc::types::Log,
    sol_types::{
        self,
        SolEvent,
    },
};
// `HandleRevealed` is one event every libid contract declares through the
// same interface; the registry's binding decodes the escrow's logs of it too.
use libid_contracts::bindings::{
    escrow::HandleEscrow,
    identity::IdentityRegistry::{
        self,
        HandleRevealed,
    },
};

use crate::nodes::{
    DisclosedHandle,
    Platform,
};
use serde_with::{
    DeserializeFromStr,
    SerializeDisplay,
};

/// Where in the chain a log sat. The pair (block, log index) is the natural
/// id the journal keys each event by.
#[derive(Debug, Clone, Copy)]
pub struct LogPosition {
    /// The block that holds the log.
    pub block_number: u64,
    /// The log's index within that block.
    pub log_index: u64,
    /// The transaction that emitted it.
    pub tx_hash: B256,
    /// The block's timestamp, in unix seconds: what orders a history across
    /// chains, whose block numbers do not compare.
    pub block_time: u64,
}

/// One decoded `IdentityRegistry` or `HandleEscrow` event. Field names track
/// the Solidity event parameters one to one, so they carry no docs of their
/// own — the contracts' natspec is the reference. Every `uint256` stays a
/// `U256`: escrowed amounts are token units, and nothing bounds them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum NamesEvent {
    /// A holder proved an identity: its two nodes, never the id or handle.
    /// The ceremony version is logged and never stored on chain, so this
    /// side is its only record.
    IdentityBound {
        holder: Address,
        id_node: B256,
        handle_node: B256,
        platform_id: B256,
        observed_at: u64,
        ceremony_version: u16,
    },
    /// A handle's text went on chain: a disclosing bind, `publish` or
    /// `reveal` on the registry, or a `depositToHandle` on the escrow, which
    /// log it the same way. `handle` is the text when it hashes to
    /// `handle_node`; the journal never holds it.
    HandleRevealed {
        platform_id: B256,
        handle_node: B256,
        handle: Option<DisclosedHandle>,
    },
    /// A holder made a handle it holds its name, replacing any name it had on
    /// any platform. The text is in the `HandleRevealed` before it.
    HandlePublished {
        holder: Address,
        platform_id: B256,
        handle_node: B256,
    },
    /// The identity behind a handle proved a different one; the old node,
    /// which `holder` held, stops resolving but keeps its observed-at
    /// watermark.
    HandleRetired {
        platform_id: B256,
        handle_node: B256,
        holder: Address,
    },
    /// A holder's name was withdrawn: by the holder, or by a bind that took
    /// or retired the handle it names.
    HandleUnpublished { holder: Address },
    /// The contract was pointed at the Proof Verifier that checks every claim
    /// and holds the version set the contract itself does not.
    ProofVerifierConfigured { verifier: Address },
    /// What a ceremony carried that the binding does not keep: the client the
    /// platform authenticated, keyed by the digest that names the ceremony.
    /// Emitted beside the `IdentityBound` of the same binding.
    CeremonyBound {
        authorization_digest: B256,
        holder: Address,
        platform_id: B256,
        client_identifier: Bytes,
    },
    /// The service fee a binding's own transaction data named was paid out.
    BindFeePaid {
        authorization_digest: B256,
        receiver: Address,
        amount: U256,
    },
    /// One of `HandleEscrow`'s events, journaled under its own kind.
    Escrow(EscrowEvent),
}

/// One decoded `HandleEscrow` event: what moves the escrow's books.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum EscrowEvent {
    /// Value escrowed for a handle node nobody holds, booked under `sender`
    /// in `round`, which the node's next `Claimed` closes. The sender's whole
    /// contribution to the slot is refundable from `refundable_at`.
    Deposited {
        handle_node: B256,
        token: Address,
        sender: Address,
        round: U256,
        amount: U256,
        refundable_at: u64,
    },
    /// `sender`'s deposit for a held node, paid straight to its holder.
    Forwarded {
        handle_node: B256,
        token: Address,
        sender: Address,
        holder: Address,
        amount: U256,
        received: U256,
    },
    /// The holder took everything held in one token, closing `round`.
    Claimed {
        handle_node: B256,
        token: Address,
        claimer: Address,
        recipient: Address,
        round: U256,
        released: U256,
        received: U256,
    },
    /// `sender` took its `round` contribution back.
    Refunded {
        handle_node: B256,
        token: Address,
        sender: Address,
        recipient: Address,
        round: U256,
        released: U256,
        received: U256,
    },
}

/// The part an address plays in an event, as an address history reports it.
/// Most are the event's own field names; the two previous holders are what a
/// bind takes from an address the event does not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SerializeDisplay, DeserializeFromStr)]
pub enum Role {
    /// The address an identity binds to: it proved, withdrew, ran the
    /// ceremony of, or paid the fee of a binding, or was paid through a
    /// handle it holds.
    Holder,
    /// Held the handle until this event took it: a bind gave it to another
    /// address, or retired it.
    PreviousHandleHolder,
    /// Held the account until this bind moved it to another address.
    PreviousIdHolder,
    /// Was paid a bind's service fee.
    FeeReceiver,
    /// Sent a deposit: escrowed it, which it may refund, did refund, or lost
    /// to the holder's claim, or paid it straight to a holder.
    Sender,
    /// Claimed what a handle it holds was sent.
    Claimer,
    /// Was paid by a claim or a refund.
    Recipient,
}

/// A stored role name this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown role {0:?}")]
pub struct UnknownRole(String);

impl Role {
    /// Every role, so a stored name parses back by the same table.
    const ALL: [Self; 7] = [
        Self::Holder,
        Self::PreviousHandleHolder,
        Self::PreviousIdHolder,
        Self::FeeReceiver,
        Self::Sender,
        Self::Claimer,
        Self::Recipient,
    ];

    /// The name stored and served: the camelCase the API spells it in.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Holder => "holder",
            Self::PreviousHandleHolder => "previousHandleHolder",
            Self::PreviousIdHolder => "previousIdHolder",
            Self::FeeReceiver => "feeReceiver",
            Self::Sender => "sender",
            Self::Claimer => "claimer",
            Self::Recipient => "recipient",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(name: &str) -> Result<Self, UnknownRole> {
        Self::ALL
            .into_iter()
            .find(|role| role.as_str() == name)
            .ok_or_else(|| UnknownRole(name.to_string()))
    }
}

/// Which event a journal row holds: one per [`NamesEvent`] and
/// [`EscrowEvent`] variant of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// [`NamesEvent::IdentityBound`].
    IdentityBound,
    /// [`NamesEvent::HandleRevealed`].
    HandleRevealed,
    /// [`NamesEvent::HandlePublished`].
    HandlePublished,
    /// [`NamesEvent::HandleRetired`].
    HandleRetired,
    /// [`NamesEvent::HandleUnpublished`].
    HandleUnpublished,
    /// [`NamesEvent::ProofVerifierConfigured`].
    ProofVerifierConfigured,
    /// [`NamesEvent::CeremonyBound`].
    CeremonyBound,
    /// [`NamesEvent::BindFeePaid`].
    BindFeePaid,
    /// [`EscrowEvent::Deposited`].
    Deposited,
    /// [`EscrowEvent::Forwarded`].
    Forwarded,
    /// [`EscrowEvent::Claimed`].
    Claimed,
    /// [`EscrowEvent::Refunded`].
    Refunded,
}

/// A stored kind this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown event kind {0:?}")]
pub struct UnknownKind(String);

impl EventKind {
    /// Every kind, so a stored name parses back by the same table.
    const ALL: [Self; 12] = [
        Self::IdentityBound,
        Self::HandleRevealed,
        Self::HandlePublished,
        Self::HandleRetired,
        Self::HandleUnpublished,
        Self::ProofVerifierConfigured,
        Self::CeremonyBound,
        Self::BindFeePaid,
        Self::Deposited,
        Self::Forwarded,
        Self::Claimed,
        Self::Refunded,
    ];

    /// The name the journal stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdentityBound => "identity_bound",
            Self::HandleRevealed => "handle_revealed",
            Self::HandlePublished => "handle_published",
            Self::HandleRetired => "handle_retired",
            Self::HandleUnpublished => "handle_unpublished",
            Self::ProofVerifierConfigured => "proof_verifier_configured",
            Self::CeremonyBound => "ceremony_bound",
            Self::BindFeePaid => "bind_fee_paid",
            Self::Deposited => "deposited",
            Self::Forwarded => "forwarded",
            Self::Claimed => "claimed",
            Self::Refunded => "refunded",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = UnknownKind;

    fn from_str(name: &str) -> Result<Self, UnknownKind> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == name)
            .ok_or_else(|| UnknownKind(name.to_string()))
    }
}

impl NamesEvent {
    /// Which event this is.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::IdentityBound { .. } => EventKind::IdentityBound,
            Self::HandleRevealed { .. } => EventKind::HandleRevealed,
            Self::HandlePublished { .. } => EventKind::HandlePublished,
            Self::HandleRetired { .. } => EventKind::HandleRetired,
            Self::HandleUnpublished { .. } => EventKind::HandleUnpublished,
            Self::ProofVerifierConfigured { .. } => EventKind::ProofVerifierConfigured,
            Self::CeremonyBound { .. } => EventKind::CeremonyBound,
            Self::BindFeePaid { .. } => EventKind::BindFeePaid,
            Self::Escrow(event) => event.kind(),
        }
    }

    /// The handle node the event names itself, when it names one.
    pub fn handle_node(&self) -> Option<B256> {
        match self {
            Self::IdentityBound { handle_node, .. }
            | Self::HandleRevealed { handle_node, .. }
            | Self::HandlePublished { handle_node, .. }
            | Self::HandleRetired { handle_node, .. }
            | Self::Escrow(
                EscrowEvent::Deposited { handle_node, .. }
                | EscrowEvent::Forwarded { handle_node, .. }
                | EscrowEvent::Claimed { handle_node, .. }
                | EscrowEvent::Refunded { handle_node, .. },
            ) => Some(*handle_node),
            Self::HandleUnpublished { .. }
            | Self::ProofVerifierConfigured { .. }
            | Self::CeremonyBound { .. }
            | Self::BindFeePaid { .. } => None,
        }
    }

    /// The addresses the event itself names, each with the part it plays; the
    /// apply path finds the ones it involves without naming them.
    pub fn parties(&self) -> Vec<(Address, Role)> {
        match self {
            Self::IdentityBound { holder, .. }
            | Self::HandlePublished { holder, .. }
            | Self::HandleUnpublished { holder, .. }
            | Self::CeremonyBound { holder, .. } => vec![(*holder, Role::Holder)],
            Self::HandleRetired { holder, .. } => {
                vec![(*holder, Role::PreviousHandleHolder)]
            }
            Self::BindFeePaid { receiver, .. } => vec![(*receiver, Role::FeeReceiver)],
            Self::HandleRevealed { .. } | Self::ProofVerifierConfigured { .. } => {
                Vec::new()
            }
            Self::Escrow(event) => event.parties(),
        }
    }

    /// The handle text the event revealed, with its platform and node.
    pub fn disclosure(&self) -> Option<(B256, B256, &DisclosedHandle)> {
        match self {
            Self::HandleRevealed {
                platform_id,
                handle_node,
                handle,
                ..
            } => Some((*platform_id, *handle_node, handle.as_ref()?)),
            _ => None,
        }
    }
}

#[cfg(test)]
impl NamesEvent {
    /// The event as its journal row reads back: disclosed text dropped.
    pub(crate) fn without_text(mut self) -> Self {
        if let Self::HandleRevealed { handle, .. } = &mut self {
            *handle = None
        }
        self
    }
}

impl EscrowEvent {
    /// Which event this is.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::Deposited { .. } => EventKind::Deposited,
            Self::Forwarded { .. } => EventKind::Forwarded,
            Self::Claimed { .. } => EventKind::Claimed,
            Self::Refunded { .. } => EventKind::Refunded,
        }
    }

    /// The addresses the event names, each with the part it plays.
    fn parties(&self) -> Vec<(Address, Role)> {
        match self {
            Self::Deposited { sender, .. } => vec![(*sender, Role::Sender)],
            Self::Forwarded { sender, holder, .. } => {
                vec![(*sender, Role::Sender), (*holder, Role::Holder)]
            }
            Self::Claimed {
                claimer, recipient, ..
            } => vec![(*claimer, Role::Claimer), (*recipient, Role::Recipient)],
            Self::Refunded {
                sender, recipient, ..
            } => vec![(*sender, Role::Sender), (*recipient, Role::Recipient)],
        }
    }
}

/// Why a log from the watched contract could not be decoded. Distinct from
/// `decode`'s `Ok(None)` — an unknown topic is legal and survivable, while
/// these are not: a caller must fail the window on them, because skipping a
/// log the contract really emitted would silently fork the read model from
/// the chain.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The RPC returned a log without a field every mined log carries.
    #[error("log without a {0}")]
    MissingField(&'static str),
    /// A known topic whose payload did not decode.
    #[error("{event}: {source}")]
    Payload {
        /// The event the topic names.
        event: &'static str,
        /// The decoder's reason.
        source: sol_types::Error,
    },
}

/// The payload of a log whose topic named `E`. Failing here is
/// [`DecodeError::Payload`], never `Ok(None)`: the topic was recognized.
fn payload_of<E: SolEvent>(log: &Log, event: &'static str) -> Result<E, DecodeError> {
    log.log_decode::<E>()
        .map(|decoded| decoded.inner.data)
        .map_err(|source| DecodeError::Payload { event, source })
}

/// The text a `HandleRevealed` carried, when it is a handle hashing to the
/// node the event names. Other text is dropped and logged without itself.
fn disclosed(
    platform_id: B256,
    handle_node: B256,
    text: &str,
) -> Option<DisclosedHandle> {
    let Some(platform) = Platform::known_of(platform_id) else {
        tracing::warn!(
            %platform_id,
            "revealed text on a platform this build does not know; not stored"
        );
        return None;
    };
    let verified = DisclosedHandle::verify(platform, handle_node, text);
    if verified.is_none() {
        tracing::warn!(
            %platform_id,
            %handle_node,
            "revealed text is not a handle hashing to its node; not stored"
        );
    }
    verified
}

impl NamesEvent {
    /// The `HandleRevealed` a log carries, whichever libid contract wrote it:
    /// they share the event, topic and all.
    fn revealed(log: &Log) -> Result<Self, DecodeError> {
        let d: HandleRevealed = payload_of(log, "HandleRevealed")?;
        Ok(Self::HandleRevealed {
            platform_id: d.platformId,
            handle_node: d.handleNode,
            handle: disclosed(d.platformId, d.handleNode, &d.handle),
        })
    }
}

impl LogPosition {
    /// Where a mined log sat. An RPC that omits the block's timestamp fails
    /// the window rather than writing a history with no time in it.
    fn of(log: &Log) -> Result<Self, DecodeError> {
        Ok(Self {
            block_number: log
                .block_number
                .ok_or(DecodeError::MissingField("block number"))?,
            log_index: log
                .log_index
                .ok_or(DecodeError::MissingField("log index"))?,
            tx_hash: log
                .transaction_hash
                .ok_or(DecodeError::MissingField("transaction hash"))?,
            block_time: log
                .block_timestamp
                .ok_or(DecodeError::MissingField("block timestamp"))?,
        })
    }
}

/// Decode one log from `IdentityRegistry`. `Ok(None)` is a topic this indexer
/// does not know — legal, because an upgraded contract may emit new events
/// before this build learns them. A log that names a known topic but fails to
/// decode is an error: skipping it would silently fork the read model from
/// the chain.
pub fn decode_registry(
    log: &Log,
) -> Result<Option<(NamesEvent, LogPosition)>, DecodeError> {
    let position = LogPosition::of(log)?;
    let Some(&topic0) = log.topic0() else {
        return Ok(None);
    };

    let event = if topic0 == IdentityRegistry::IdentityBound::SIGNATURE_HASH {
        let d: IdentityRegistry::IdentityBound = payload_of(log, "IdentityBound")?;
        NamesEvent::IdentityBound {
            holder: d.holder,
            id_node: d.idNode,
            handle_node: d.handleNode,
            platform_id: d.platformId,
            observed_at: d.observedAt,
            ceremony_version: d.ceremonyVersion,
        }
    } else if topic0 == HandleRevealed::SIGNATURE_HASH {
        NamesEvent::revealed(log)?
    } else if topic0 == IdentityRegistry::HandlePublished::SIGNATURE_HASH {
        let d: IdentityRegistry::HandlePublished = payload_of(log, "HandlePublished")?;
        NamesEvent::HandlePublished {
            holder: d.holder,
            platform_id: d.platformId,
            handle_node: d.handleNode,
        }
    } else if topic0 == IdentityRegistry::HandleRetired::SIGNATURE_HASH {
        let d: IdentityRegistry::HandleRetired = payload_of(log, "HandleRetired")?;
        NamesEvent::HandleRetired {
            platform_id: d.platformId,
            handle_node: d.handleNode,
            holder: d.holder,
        }
    } else if topic0 == IdentityRegistry::HandleUnpublished::SIGNATURE_HASH {
        let d: IdentityRegistry::HandleUnpublished =
            payload_of(log, "HandleUnpublished")?;
        NamesEvent::HandleUnpublished { holder: d.holder }
    } else if topic0 == IdentityRegistry::ProofVerifierConfigured::SIGNATURE_HASH {
        let d: IdentityRegistry::ProofVerifierConfigured =
            payload_of(log, "ProofVerifierConfigured")?;
        NamesEvent::ProofVerifierConfigured {
            verifier: d.verifier,
        }
    } else if topic0 == IdentityRegistry::CeremonyBound::SIGNATURE_HASH {
        let d: IdentityRegistry::CeremonyBound = payload_of(log, "CeremonyBound")?;
        NamesEvent::CeremonyBound {
            authorization_digest: d.authorizationDigest,
            holder: d.holder,
            platform_id: d.platformId,
            client_identifier: d.clientIdentifier,
        }
    } else if topic0 == IdentityRegistry::BindFeePaid::SIGNATURE_HASH {
        let d: IdentityRegistry::BindFeePaid = payload_of(log, "BindFeePaid")?;
        NamesEvent::BindFeePaid {
            authorization_digest: d.authorizationDigest,
            receiver: d.receiver,
            amount: d.amount,
        }
    } else {
        return Ok(None);
    };

    Ok(Some((event, position)))
}

/// Decode one log from `HandleEscrow`, under the same rules as
/// [`decode_registry`]: an unknown topic is `Ok(None)`, a known one that does
/// not decode is an error. The proxy's own events — ownership, upgrades,
/// initialization — are unknown topics here. A `depositToHandle`'s
/// `HandleRevealed` decodes as the registry's does.
pub fn decode_escrow(
    log: &Log,
) -> Result<Option<(NamesEvent, LogPosition)>, DecodeError> {
    let position = LogPosition::of(log)?;
    let Some(&topic0) = log.topic0() else {
        return Ok(None);
    };
    if topic0 == HandleRevealed::SIGNATURE_HASH {
        return Ok(Some((NamesEvent::revealed(log)?, position)));
    }

    let event = if topic0 == HandleEscrow::Deposited::SIGNATURE_HASH {
        let d: HandleEscrow::Deposited = payload_of(log, "Deposited")?;
        EscrowEvent::Deposited {
            handle_node: d.handleNode,
            token: d.token,
            sender: d.sender,
            round: d.round,
            amount: d.amount,
            refundable_at: d.refundableAt,
        }
    } else if topic0 == HandleEscrow::Forwarded::SIGNATURE_HASH {
        let d: HandleEscrow::Forwarded = payload_of(log, "Forwarded")?;
        EscrowEvent::Forwarded {
            handle_node: d.handleNode,
            token: d.token,
            sender: d.sender,
            holder: d.holder,
            amount: d.amount,
            received: d.received,
        }
    } else if topic0 == HandleEscrow::Claimed::SIGNATURE_HASH {
        let d: HandleEscrow::Claimed = payload_of(log, "Claimed")?;
        EscrowEvent::Claimed {
            handle_node: d.handleNode,
            token: d.token,
            claimer: d.claimer,
            recipient: d.recipient,
            round: d.round,
            released: d.released,
            received: d.received,
        }
    } else if topic0 == HandleEscrow::Refunded::SIGNATURE_HASH {
        let d: HandleEscrow::Refunded = payload_of(log, "Refunded")?;
        EscrowEvent::Refunded {
            handle_node: d.handleNode,
            token: d.token,
            sender: d.sender,
            recipient: d.recipient,
            round: d.round,
            released: d.released,
            received: d.received,
        }
    } else {
        return Ok(None);
    };

    Ok(Some((NamesEvent::Escrow(event), position)))
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives,
        primitives::{
            b256,
            LogData,
        },
    };

    use super::*;
    use crate::nodes::KnownPlatform;

    const HOLDER: Address = Address::repeat_byte(0xA1);

    /// A log carrying `data`, as the RPC returns it once mined.
    fn mined(data: LogData) -> Log {
        Log {
            inner: primitives::Log {
                address: Address::repeat_byte(0x11),
                data,
            },
            block_number: Some(7),
            log_index: Some(3),
            transaction_hash: Some(B256::repeat_byte(0x22)),
            block_timestamp: Some(1_700_000_000),
            ..Log::default()
        }
    }

    /// Fixed points from `cast keccak` over the event signatures in
    /// `IdentityRegistry.sol`, so a crate binding that drifts from the contract
    /// fails against numbers this code never produced.
    #[test]
    fn topics_match_the_contract() {
        assert_eq!(
            IdentityRegistry::IdentityBound::SIGNATURE_HASH,
            b256!("7de04595ed86c830556fbd7a7b60045145ee35b154a004b0c530ef91a4bd836c")
        );
        assert_eq!(
            IdentityRegistry::HandlePublished::SIGNATURE_HASH,
            b256!("04a31fa5777bb2c9381e5ebd6b6f967c2094d3874b1c7789dc15a80c7dcbcafb")
        );
        assert_eq!(
            HandleRevealed::SIGNATURE_HASH,
            b256!("8ba0ebc5838d01aae10591d451b810b02cbd938a799162a7dda37f1d37b5e9f7")
        );
        assert_eq!(
            IdentityRegistry::HandleRetired::SIGNATURE_HASH,
            b256!("278e2122d24782b703f05416ac53a9750d67bcf6fa1603b94ec95660821a8914")
        );
        assert_eq!(
            IdentityRegistry::CeremonyBound::SIGNATURE_HASH,
            b256!("f0f0b831e902ded46acfd6caf87649edb445c2e2733992b2e79cd1420719c19c")
        );
        assert_eq!(
            IdentityRegistry::BindFeePaid::SIGNATURE_HASH,
            b256!("471f5d0665e5719861746aa9077ef4997ae3d84cad4f349f5e7cc2cfe74be599")
        );
        assert_eq!(
            IdentityRegistry::ProofVerifierConfigured::SIGNATURE_HASH,
            b256!("01970f3cbef68f8ac95615ab5c75299e421a83a787173408d693928ed40b6574")
        );
        assert_eq!(
            IdentityRegistry::HandleUnpublished::SIGNATURE_HASH,
            b256!("916ce5f64b56cb8a45184ccbfe46b3eb4a38158e9cb0defe417c42a9bc42fda5")
        );
    }

    #[test]
    fn handle_unpublished_decodes() {
        let log = mined(
            IdentityRegistry::HandleUnpublished { holder: HOLDER }.encode_log_data(),
        );
        let (event, position) = decode_registry(&log).unwrap().expect("a known topic");
        assert!(
            matches!(
                event,
                NamesEvent::HandleUnpublished { holder } if holder == HOLDER
            ),
            "{event:?}"
        );
        assert_eq!(event.kind(), EventKind::HandleUnpublished);
        assert_eq!((position.block_number, position.log_index), (7, 3));
        assert_eq!(position.block_time, 1_700_000_000);
    }

    /// The same fixed points for `HandleEscrow.sol`'s own four events, and
    /// the `HandleRevealed` it shares with the registry.
    #[test]
    fn escrow_topics_match_the_contract() {
        assert_eq!(
            HandleEscrow::HandleRevealed::SIGNATURE_HASH,
            HandleRevealed::SIGNATURE_HASH
        );
        assert_eq!(
            HandleEscrow::Deposited::SIGNATURE_HASH,
            b256!("590730108e214366abd2912d060c6195480c59b924619145601d0d07ab14a61d")
        );
        assert_eq!(
            HandleEscrow::Forwarded::SIGNATURE_HASH,
            b256!("afdf4d834a7c59206d8f48e1bb08e8625d11809b936b1eea92c3333534bdf7c3")
        );
        assert_eq!(
            HandleEscrow::Claimed::SIGNATURE_HASH,
            b256!("1250cc5278315bb2cb7738cc5a09976c58b02915c0276c970e7f29516954cce6")
        );
        assert_eq!(
            HandleEscrow::Refunded::SIGNATURE_HASH,
            b256!("b7d023fdcfc8cdb1aeb0fcf141fb12597f076f2d9cfa676aba35f34489d52e4e")
        );
    }

    #[test]
    fn a_deposit_decodes_with_its_sender_and_refund_date() {
        let node = B256::repeat_byte(0x0D);
        let token = Address::repeat_byte(0xEE);
        let payer = Address::repeat_byte(0xB0);
        let log = mined(
            HandleEscrow::Deposited {
                handleNode: node,
                token,
                sender: payer,
                round: U256::from(2u64),
                amount: U256::MAX,
                refundableAt: u64::MAX,
            }
            .encode_log_data(),
        );
        let (event, _) = decode_escrow(&log).unwrap().expect("a known topic");
        assert_eq!(event.kind(), EventKind::Deposited);
        assert_eq!(event.parties(), [(payer, Role::Sender)]);
        assert!(matches!(
            event,
            NamesEvent::Escrow(EscrowEvent::Deposited { refundable_at, .. })
                if refundable_at == u64::MAX
        ));
    }

    /// An escrow topic means nothing from the registry, and the reverse: each
    /// contract's logs are decoded against its own events, and the
    /// `HandleRevealed` they share.
    #[test]
    fn each_decoder_knows_only_its_own_contract() {
        let escrow_log = mined(
            HandleEscrow::Refunded {
                handleNode: B256::ZERO,
                token: Address::ZERO,
                sender: HOLDER,
                recipient: HOLDER,
                round: U256::ZERO,
                released: U256::from(1u64),
                received: U256::from(1u64),
            }
            .encode_log_data(),
        );
        assert!(decode_registry(&escrow_log).unwrap().is_none());
        let registry_log = mined(
            IdentityRegistry::HandleUnpublished { holder: HOLDER }.encode_log_data(),
        );
        assert!(decode_escrow(&registry_log).unwrap().is_none());
    }

    #[test]
    fn a_log_without_its_block_time_fails_instead_of_dating_nothing() {
        let mut log = mined(
            IdentityRegistry::HandleUnpublished { holder: HOLDER }.encode_log_data(),
        );
        log.block_timestamp = None;
        assert!(matches!(
            decode_registry(&log),
            Err(DecodeError::MissingField("block timestamp"))
        ));
    }

    /// Revealed text decodes only when it hashes to the node beside it.
    #[test]
    fn a_revealed_handle_keeps_its_text_only_when_it_hashes_to_the_node() {
        let x = KnownPlatform::X;
        let node = x.handle_node("alice_1").unwrap();
        let revealed = |handle: &str| {
            let log = mined(
                HandleRevealed {
                    platformId: x.id(),
                    handleNode: node,
                    handle: handle.into(),
                }
                .encode_log_data(),
            );
            match decode_registry(&log).unwrap().expect("a known topic").0 {
                NamesEvent::HandleRevealed { handle, .. } => handle,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            revealed("Alice_1").as_ref().map(DisclosedHandle::as_str),
            Some("alice_1")
        );
        assert_eq!(revealed("bob"), None);
    }

    /// The escrow's `HandleRevealed` is the registry's: the same event, which
    /// names no address.
    #[test]
    fn an_escrow_reveal_decodes_as_the_registrys() {
        let x = KnownPlatform::X;
        let node = x.handle_node("bob_1").unwrap();
        let log = mined(
            HandleEscrow::HandleRevealed {
                platformId: x.id(),
                handleNode: node,
                handle: "Bob_1".into(),
            }
            .encode_log_data(),
        );
        let (from_escrow, _) = decode_escrow(&log).unwrap().expect("a known topic");
        let (from_registry, _) = decode_registry(&log).unwrap().expect("a known topic");
        assert_eq!(from_escrow, from_registry);
        assert_eq!(from_escrow.parties(), []);
        assert_eq!(
            from_escrow
                .disclosure()
                .map(|(_, n, text)| (n, text.as_str())),
            Some((node, "bob_1"))
        );
    }

    #[test]
    fn every_role_parses_back_from_its_name() {
        for role in Role::ALL {
            assert_eq!(role.as_str().parse::<Role>().ok(), Some(role));
        }
        assert!("owner".parse::<Role>().is_err());
    }
}
