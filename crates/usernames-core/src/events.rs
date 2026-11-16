//! Decoding `IdentityRegistry`, `HandleEscrow` and `ReferralSplitter` logs
//! into one typed stream.
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
    referral::ReferralSplitter,
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

/// One decoded `IdentityRegistry`, `HandleEscrow` or `ReferralSplitter`
/// event. Field names track
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
    /// A bind paid its fee to the referral splitter naming a referrer's
    /// handle; `share` is the referrer's part and `outcome` what became of
    /// it. A paid share's escrow event comes before this in the transaction.
    Referred {
        referrer_node: B256,
        binder: Address,
        session_id: B256,
        referrer_platform_id: B256,
        platform_id: B256,
        handle_node: B256,
        fee: U256,
        share: U256,
        outcome: Outcome,
    },
    /// The referral splitter set what a bind through it pays: exactly `fee`,
    /// with a referral or without.
    BindFeeSet { fee: U256 },
}

/// What became of a referrer's share, as the splitter logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SerializeDisplay, DeserializeFromStr)]
pub enum Outcome {
    /// Deposited with the escrow, which forwarded it to the referrer's
    /// holder or holds it for the node.
    Paid,
    /// The binder holds the referrer's node; the treasury took the share.
    SelfReferral,
    /// The escrow refused the deposit; the treasury took the share.
    Refused,
}

/// A stored outcome name this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown referral outcome {0:?}")]
pub struct UnknownOutcome(String);

impl Outcome {
    /// Every outcome in the splitter's enum order, so a logged value and a
    /// stored name each read back by the same table.
    pub const ALL: [Self; 3] = [Self::Paid, Self::SelfReferral, Self::Refused];

    /// The outcome a `Referred` log carries as its enum's `uint8`.
    fn of(value: u8) -> Result<Self, DecodeError> {
        Self::ALL
            .get(usize::from(value))
            .copied()
            .ok_or(DecodeError::Outcome(value))
    }

    /// The name stored and served.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paid => "paid",
            Self::SelfReferral => "self_referral",
            Self::Refused => "refused",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Outcome {
    type Err = UnknownOutcome;

    fn from_str(name: &str) -> Result<Self, UnknownOutcome> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == name)
            .ok_or_else(|| UnknownOutcome(name.to_string()))
    }
}

/// Where a paid share went: forwarded to the referrer's holder, or held by
/// the escrow until the node's holder claims it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SerializeDisplay, DeserializeFromStr)]
pub enum Delivery {
    /// Paid straight to the referrer's holder on this chain.
    Forwarded,
    /// Held for the referrer's node, which nobody held on this chain.
    Held,
    /// Held, then claimed by the node's holder.
    Claimed,
}

/// A stored delivery name this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown delivery {0:?}")]
pub struct UnknownDelivery(String);

impl Delivery {
    /// Every delivery, so a stored name parses back by the same table.
    pub const ALL: [Self; 3] = [Self::Forwarded, Self::Held, Self::Claimed];

    /// The name stored and served.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forwarded => "forwarded",
            Self::Held => "held",
            Self::Claimed => "claimed",
        }
    }
}

impl fmt::Display for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Delivery {
    type Err = UnknownDelivery;

    fn from_str(name: &str) -> Result<Self, UnknownDelivery> {
        Self::ALL
            .into_iter()
            .find(|delivery| delivery.as_str() == name)
            .ok_or_else(|| UnknownDelivery(name.to_string()))
    }
}

/// What an escrow event did with the referral splitter's shares for one
/// node: a deposit forwarded or held in a slot's round, or every share that
/// round held claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareMove {
    /// The referrer's node.
    pub handle_node: B256,
    /// The token the shares are in.
    pub token: Address,
    /// The slot's round; none for a forwarded share, which no slot holds.
    pub round: Option<U256>,
    /// Where the move leaves the shares.
    pub delivery: Delivery,
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
    pub const ALL: [Self; 7] = [
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
    /// [`NamesEvent::Referred`].
    Referred,
    /// [`NamesEvent::BindFeeSet`].
    BindFeeSet,
}

/// A stored kind this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown event kind {0:?}")]
pub struct UnknownKind(String);

impl EventKind {
    /// Every kind, so a stored name parses back by the same table.
    pub const ALL: [Self; 14] = [
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
        Self::Referred,
        Self::BindFeeSet,
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
            Self::Referred => "referred",
            Self::BindFeeSet => "bind_fee_set",
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
            Self::Referred { .. } => EventKind::Referred,
            Self::BindFeeSet { .. } => EventKind::BindFeeSet,
        }
    }

    /// The handle node the event names itself, when it names one: a
    /// referral's is the handle bound, not the referrer's.
    pub fn handle_node(&self) -> Option<B256> {
        match self {
            Self::IdentityBound { handle_node, .. }
            | Self::HandleRevealed { handle_node, .. }
            | Self::HandlePublished { handle_node, .. }
            | Self::HandleRetired { handle_node, .. }
            | Self::Referred { handle_node, .. }
            | Self::Escrow(
                EscrowEvent::Deposited { handle_node, .. }
                | EscrowEvent::Forwarded { handle_node, .. }
                | EscrowEvent::Claimed { handle_node, .. }
                | EscrowEvent::Refunded { handle_node, .. },
            ) => Some(*handle_node),
            Self::HandleUnpublished { .. }
            | Self::ProofVerifierConfigured { .. }
            | Self::CeremonyBound { .. }
            | Self::BindFeePaid { .. }
            | Self::BindFeeSet { .. } => None,
        }
    }

    /// The addresses the event itself names, each with the part it plays; the
    /// apply path finds the ones it involves without naming them.
    pub fn parties(&self) -> Vec<(Address, Role)> {
        match self {
            Self::IdentityBound { holder, .. }
            | Self::HandlePublished { holder, .. }
            | Self::HandleUnpublished { holder, .. }
            | Self::CeremonyBound { holder, .. }
            | Self::Referred { binder: holder, .. } => vec![(*holder, Role::Holder)],
            Self::HandleRetired { holder, .. } => {
                vec![(*holder, Role::PreviousHandleHolder)]
            }
            Self::BindFeePaid { receiver, .. } => vec![(*receiver, Role::FeeReceiver)],
            Self::HandleRevealed { .. }
            | Self::ProofVerifierConfigured { .. }
            | Self::BindFeeSet { .. } => Vec::new(),
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

#[cfg(feature = "test-helpers")]
impl NamesEvent {
    /// The event as its journal row reads back: disclosed text dropped.
    pub fn without_text(mut self) -> Self {
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

    /// Where a deposit by `splitter` went: forwarded, or held in a round.
    /// `None` for any other event, or a deposit somebody else sent.
    pub fn deposit_by(&self, splitter: Address) -> Option<ShareMove> {
        match *self {
            Self::Forwarded {
                handle_node,
                token,
                sender,
                ..
            } if sender == splitter => Some(ShareMove {
                handle_node,
                token,
                round: None,
                delivery: Delivery::Forwarded,
            }),
            Self::Deposited {
                handle_node,
                token,
                sender,
                round,
                ..
            } if sender == splitter => Some(ShareMove {
                handle_node,
                token,
                round: Some(round),
                delivery: Delivery::Held,
            }),
            _ => None,
        }
    }

    /// The round a claim settles for the shares it held: the splitter never
    /// takes a share back, so only the node's holder settles one. `None` for
    /// any other event.
    pub fn settles(&self) -> Option<ShareMove> {
        match *self {
            Self::Claimed {
                handle_node,
                token,
                round,
                ..
            } => Some(ShareMove {
                handle_node,
                token,
                round: Some(round),
                delivery: Delivery::Claimed,
            }),
            _ => None,
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
    /// A `Referred` whose outcome is past the splitter's enum.
    #[error("Referred: outcome {0} is no Outcome")]
    Outcome(u8),
}

/// A watched log's payload, read by its topic.
trait Payload {
    /// The payload of a log whose topic named `E`. Failing here is
    /// [`DecodeError::Payload`], never `Ok(None)`: the topic was recognized.
    fn payload<E: SolEvent>(&self, event: &'static str) -> Result<E, DecodeError>;
}

impl Payload for Log {
    fn payload<E: SolEvent>(&self, event: &'static str) -> Result<E, DecodeError> {
        self.log_decode::<E>()
            .map(|decoded| decoded.inner.data)
            .map_err(|source| DecodeError::Payload { event, source })
    }
}

impl DisclosedHandle {
    /// The text a `HandleRevealed` carried, when it is a handle hashing to the
    /// node the event names. Other text is dropped and logged without itself.
    fn revealed(platform_id: B256, handle_node: B256, text: &str) -> Option<Self> {
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
}

impl NamesEvent {
    /// The `HandleRevealed` a log carries, whichever libid contract wrote it:
    /// they share the event, topic and all.
    fn from_revealed(log: &Log) -> Result<Self, DecodeError> {
        let d: HandleRevealed = log.payload("HandleRevealed")?;
        Ok(Self::HandleRevealed {
            platform_id: d.platformId,
            handle_node: d.handleNode,
            handle: DisclosedHandle::revealed(d.platformId, d.handleNode, &d.handle),
        })
    }
}

impl LogPosition {
    /// Where a mined log sat. A log still without its block's time, which
    /// [`crate::chain::Contracts::logs`] reads from the header when an
    /// endpoint leaves it out, fails the window rather than writing a
    /// history with no time in it.
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

impl NamesEvent {
    /// Decode one log from `IdentityRegistry`. `Ok(None)` is a topic this indexer
    /// does not know — legal, because an upgraded contract may emit new events
    /// before this build learns them. A log that names a known topic but fails to
    /// decode is an error: skipping it would silently fork the read model from
    /// the chain.
    pub fn from_registry(log: &Log) -> Result<Option<(Self, LogPosition)>, DecodeError> {
        let position = LogPosition::of(log)?;
        let Some(&topic0) = log.topic0() else {
            return Ok(None);
        };

        let event = if topic0 == IdentityRegistry::IdentityBound::SIGNATURE_HASH {
            let d: IdentityRegistry::IdentityBound = log.payload("IdentityBound")?;
            NamesEvent::IdentityBound {
                holder: d.holder,
                id_node: d.idNode,
                handle_node: d.handleNode,
                platform_id: d.platformId,
                observed_at: d.observedAt,
                ceremony_version: d.ceremonyVersion,
            }
        } else if topic0 == HandleRevealed::SIGNATURE_HASH {
            NamesEvent::from_revealed(log)?
        } else if topic0 == IdentityRegistry::HandlePublished::SIGNATURE_HASH {
            let d: IdentityRegistry::HandlePublished = log.payload("HandlePublished")?;
            NamesEvent::HandlePublished {
                holder: d.holder,
                platform_id: d.platformId,
                handle_node: d.handleNode,
            }
        } else if topic0 == IdentityRegistry::HandleRetired::SIGNATURE_HASH {
            let d: IdentityRegistry::HandleRetired = log.payload("HandleRetired")?;
            NamesEvent::HandleRetired {
                platform_id: d.platformId,
                handle_node: d.handleNode,
                holder: d.holder,
            }
        } else if topic0 == IdentityRegistry::HandleUnpublished::SIGNATURE_HASH {
            let d: IdentityRegistry::HandleUnpublished =
                log.payload("HandleUnpublished")?;
            NamesEvent::HandleUnpublished { holder: d.holder }
        } else if topic0 == IdentityRegistry::ProofVerifierConfigured::SIGNATURE_HASH {
            let d: IdentityRegistry::ProofVerifierConfigured =
                log.payload("ProofVerifierConfigured")?;
            NamesEvent::ProofVerifierConfigured {
                verifier: d.verifier,
            }
        } else if topic0 == IdentityRegistry::CeremonyBound::SIGNATURE_HASH {
            let d: IdentityRegistry::CeremonyBound = log.payload("CeremonyBound")?;
            NamesEvent::CeremonyBound {
                authorization_digest: d.authorizationDigest,
                holder: d.holder,
                platform_id: d.platformId,
                client_identifier: d.clientIdentifier,
            }
        } else if topic0 == IdentityRegistry::BindFeePaid::SIGNATURE_HASH {
            let d: IdentityRegistry::BindFeePaid = log.payload("BindFeePaid")?;
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
    /// [`Self::from_registry`]: an unknown topic is `Ok(None)`, a known one that does
    /// not decode is an error. The proxy's own events — ownership, upgrades,
    /// initialization — are unknown topics here. A `depositToHandle`'s
    /// `HandleRevealed` decodes as the registry's does.
    pub fn from_escrow(log: &Log) -> Result<Option<(Self, LogPosition)>, DecodeError> {
        let position = LogPosition::of(log)?;
        let Some(&topic0) = log.topic0() else {
            return Ok(None);
        };
        if topic0 == HandleRevealed::SIGNATURE_HASH {
            return Ok(Some((NamesEvent::from_revealed(log)?, position)));
        }

        let event = if topic0 == HandleEscrow::Deposited::SIGNATURE_HASH {
            let d: HandleEscrow::Deposited = log.payload("Deposited")?;
            EscrowEvent::Deposited {
                handle_node: d.handleNode,
                token: d.token,
                sender: d.sender,
                round: d.round,
                amount: d.amount,
                refundable_at: d.refundableAt,
            }
        } else if topic0 == HandleEscrow::Forwarded::SIGNATURE_HASH {
            let d: HandleEscrow::Forwarded = log.payload("Forwarded")?;
            EscrowEvent::Forwarded {
                handle_node: d.handleNode,
                token: d.token,
                sender: d.sender,
                holder: d.holder,
                amount: d.amount,
                received: d.received,
            }
        } else if topic0 == HandleEscrow::Claimed::SIGNATURE_HASH {
            let d: HandleEscrow::Claimed = log.payload("Claimed")?;
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
            let d: HandleEscrow::Refunded = log.payload("Refunded")?;
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

    /// Decode one log from the `ReferralSplitter`, under the same rules as
    /// [`Self::from_registry`]: `Referred` and `FeeSet`. The share and the
    /// treasury change what later referrals pay, which their own events carry.
    pub fn from_splitter(log: &Log) -> Result<Option<(Self, LogPosition)>, DecodeError> {
        let position = LogPosition::of(log)?;
        let Some(topic0) = log.topic0() else {
            return Ok(None);
        };
        let event = if *topic0 == ReferralSplitter::Referred::SIGNATURE_HASH {
            let d: ReferralSplitter::Referred = log.payload("Referred")?;
            NamesEvent::Referred {
                referrer_node: d.referrerNode,
                binder: d.binder,
                session_id: d.sessionId,
                referrer_platform_id: d.referrerPlatformId,
                platform_id: d.platformId,
                handle_node: d.handleNode,
                fee: d.fee,
                share: d.share,
                outcome: Outcome::of(d.outcome)?,
            }
        } else if *topic0 == ReferralSplitter::FeeSet::SIGNATURE_HASH {
            let d: ReferralSplitter::FeeSet = log.payload("FeeSet")?;
            NamesEvent::BindFeeSet { fee: d.fee }
        } else {
            return Ok(None);
        };
        Ok(Some((event, position)))
    }
}
