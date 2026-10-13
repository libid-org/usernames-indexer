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
use libid_contracts::bindings::{
    escrow::HandleEscrow,
    identity::IdentityRegistry,
};
use serde::{
    de::Error as _,
    Deserialize,
    Serialize,
};
use serde_json::Value;

use crate::nodes::{
    DisclosedHandle,
    Platform,
};
use serde_with::{
    serde_as,
    DeserializeFromStr,
    DisplayFromStr,
    SerializeDisplay,
};

/// Where in the chain a log sat. The pair (block, log index) is the natural
/// id every table keys provenance by.
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

/// One decoded `IdentityRegistry` or `HandleEscrow` event, and the journal's
/// record of it: `kind` names the variant, the fields keep their camelCase
/// names. Field names track the Solidity event parameters one to one, so
/// they carry no docs of their own — the contracts' natspec is the
/// reference. Every `uint256` stays a `U256`, a decimal string in the
/// journal: escrowed amounts are token units, and nothing bounds them.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
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
    /// A holder disclosed its handle as its name on a platform, by a
    /// disclosing bind or by `publish`. `handle` is the text when it hashes to
    /// `handle_node`; the journal never holds it.
    HandlePublished {
        holder: Address,
        platform_id: B256,
        handle_node: B256,
        #[serde(skip)]
        handle: Option<DisclosedHandle>,
    },
    /// The identity behind a handle proved a different one; the old node,
    /// which `holder` held, stops resolving but keeps its observed-at
    /// watermark.
    HandleRetired {
        platform_id: B256,
        handle_node: B256,
        holder: Address,
    },
    /// A holder's published handle was withdrawn: by the holder, or by a
    /// bind that took the handle from it.
    HandleUnpublished { holder: Address, platform_id: B256 },
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
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
    },
    /// One of `HandleEscrow`'s events, journaled under its own kind.
    #[serde(untagged)]
    Escrow(EscrowEvent),
}

/// One decoded `HandleEscrow` event: what moves the escrow's books.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[allow(missing_docs)]
pub enum EscrowEvent {
    /// Value escrowed for a handle node nobody holds, booked under `refund_to`
    /// in `round`, which the node's next `Claimed` closes.
    Deposited {
        handle_node: B256,
        token: Address,
        refund_to: Address,
        depositor: Address,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
    },
    /// A deposit named its handle as text, after its `Deposited` or
    /// `Forwarded`. `handle` is the text, folded, when it hashes to
    /// `handle_node`; the journal never holds it.
    DepositedToHandle {
        handle_node: B256,
        platform_id: B256,
        #[serde(skip)]
        handle: Option<DisclosedHandle>,
    },
    /// A deposit for a held node, paid straight to its holder.
    Forwarded {
        handle_node: B256,
        token: Address,
        depositor: Address,
        holder: Address,
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
        #[serde_as(as = "DisplayFromStr")]
        received: U256,
    },
    /// The holder took everything held in one token, closing `round`.
    Claimed {
        handle_node: B256,
        token: Address,
        claimer: Address,
        recipient: Address,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        released: U256,
        #[serde_as(as = "DisplayFromStr")]
        received: U256,
    },
    /// `refund_to` took its `round` contribution back.
    Refunded {
        handle_node: B256,
        token: Address,
        refund_to: Address,
        recipient: Address,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        released: U256,
        #[serde_as(as = "DisplayFromStr")]
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
    /// Paid a deposit.
    Depositor,
    /// May refund an escrowed deposit, did, or lost the right when the
    /// holder claimed it.
    RefundTo,
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
    const ALL: [Self; 8] = [
        Self::Holder,
        Self::PreviousHandleHolder,
        Self::PreviousIdHolder,
        Self::FeeReceiver,
        Self::Depositor,
        Self::RefundTo,
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
            Self::Depositor => "depositor",
            Self::RefundTo => "refundTo",
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

impl NamesEvent {
    /// The journal's `kind` discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IdentityBound { .. } => "identity_bound",
            Self::HandlePublished { .. } => "handle_published",
            Self::HandleRetired { .. } => "handle_retired",
            Self::HandleUnpublished { .. } => "handle_unpublished",
            Self::ProofVerifierConfigured { .. } => "proof_verifier_configured",
            Self::CeremonyBound { .. } => "ceremony_bound",
            Self::BindFeePaid { .. } => "bind_fee_paid",
            Self::Escrow(event) => event.kind(),
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
            Self::ProofVerifierConfigured { .. } => Vec::new(),
            Self::Escrow(event) => event.parties(),
        }
    }

    /// The handle text the event disclosed, with its platform and node.
    pub fn disclosure(&self) -> Option<(B256, B256, &DisclosedHandle)> {
        match self {
            Self::HandlePublished {
                platform_id,
                handle_node,
                handle,
                ..
            }
            | Self::Escrow(EscrowEvent::DepositedToHandle {
                platform_id,
                handle_node,
                handle,
            }) => Some((*platform_id, *handle_node, handle.as_ref()?)),
            _ => None,
        }
    }

    /// The journal payload: the fields under their camelCase names, without
    /// the `kind` its own column holds.
    pub fn payload(&self) -> Result<Value, serde_json::Error> {
        match serde_json::to_value(self)? {
            Value::Object(mut fields) => {
                fields.remove("kind");
                Ok(Value::Object(fields))
            }
            _ => Err(serde_json::Error::custom(
                "an event serializes to an object",
            )),
        }
    }

    /// The event a journal row holds: its kind, and the payload
    /// [`NamesEvent::payload`] wrote.
    pub fn from_journal(kind: &str, payload: Value) -> Result<Self, serde_json::Error> {
        let Value::Object(mut fields) = payload else {
            return Err(serde_json::Error::custom("a journal payload is an object"));
        };
        fields.insert("kind".into(), Value::String(kind.into()));
        serde_json::from_value(Value::Object(fields))
    }
}

#[cfg(test)]
impl NamesEvent {
    /// The event as its journal row reads back: disclosed text dropped.
    fn without_text(mut self) -> Self {
        match &mut self {
            Self::HandlePublished { handle, .. }
            | Self::Escrow(EscrowEvent::DepositedToHandle { handle, .. }) => {
                *handle = None
            }
            _ => {}
        }
        self
    }
}

impl EscrowEvent {
    /// The journal's `kind` discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Deposited { .. } => "deposited",
            Self::DepositedToHandle { .. } => "deposited_to_handle",
            Self::Forwarded { .. } => "forwarded",
            Self::Claimed { .. } => "claimed",
            Self::Refunded { .. } => "refunded",
        }
    }

    /// The addresses the event names, each with the part it plays.
    fn parties(&self) -> Vec<(Address, Role)> {
        match self {
            Self::Deposited {
                refund_to,
                depositor,
                ..
            } => vec![(*depositor, Role::Depositor), (*refund_to, Role::RefundTo)],
            Self::DepositedToHandle { .. } => Vec::new(),
            Self::Forwarded {
                depositor, holder, ..
            } => vec![(*depositor, Role::Depositor), (*holder, Role::Holder)],
            Self::Claimed {
                claimer, recipient, ..
            } => vec![(*claimer, Role::Claimer), (*recipient, Role::Recipient)],
            Self::Refunded {
                refund_to,
                recipient,
                ..
            } => vec![(*refund_to, Role::RefundTo), (*recipient, Role::Recipient)],
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

/// The text an event disclosed, when it hashes to the node the event names.
/// Text that does not is dropped and logged without itself.
fn disclosed(
    event: &'static str,
    platform_id: B256,
    handle_node: B256,
    text: &str,
) -> Option<DisclosedHandle> {
    let Some(platform) = Platform::known_of(platform_id) else {
        tracing::warn!(
            event,
            %platform_id,
            "disclosed text on a platform this build does not know; not stored"
        );
        return None;
    };
    let verified = DisclosedHandle::verify(platform, handle_node, text);
    if verified.is_none() {
        tracing::warn!(
            event,
            %platform_id,
            %handle_node,
            "disclosed text does not hash to its node; not stored"
        );
    }
    verified
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
    } else if topic0 == IdentityRegistry::HandlePublished::SIGNATURE_HASH {
        let d: IdentityRegistry::HandlePublished = payload_of(log, "HandlePublished")?;
        NamesEvent::HandlePublished {
            holder: d.holder,
            platform_id: d.platformId,
            handle_node: d.handleNode,
            handle: disclosed("HandlePublished", d.platformId, d.handleNode, &d.handle),
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
        NamesEvent::HandleUnpublished {
            holder: d.holder,
            platform_id: d.platformId,
        }
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
/// initialization — are unknown topics here.
pub fn decode_escrow(
    log: &Log,
) -> Result<Option<(NamesEvent, LogPosition)>, DecodeError> {
    let position = LogPosition::of(log)?;
    let Some(&topic0) = log.topic0() else {
        return Ok(None);
    };

    let event = if topic0 == HandleEscrow::Deposited::SIGNATURE_HASH {
        let d: HandleEscrow::Deposited = payload_of(log, "Deposited")?;
        EscrowEvent::Deposited {
            handle_node: d.handleNode,
            token: d.token,
            refund_to: d.refundTo,
            depositor: d.depositor,
            round: d.round,
            amount: d.amount,
        }
    } else if topic0 == HandleEscrow::DepositedToHandle::SIGNATURE_HASH {
        let d: HandleEscrow::DepositedToHandle = payload_of(log, "DepositedToHandle")?;
        EscrowEvent::DepositedToHandle {
            handle_node: d.handleNode,
            platform_id: d.platformId,
            handle: disclosed("DepositedToHandle", d.platformId, d.handleNode, &d.handle),
        }
    } else if topic0 == HandleEscrow::Forwarded::SIGNATURE_HASH {
        let d: HandleEscrow::Forwarded = payload_of(log, "Forwarded")?;
        EscrowEvent::Forwarded {
            handle_node: d.handleNode,
            token: d.token,
            depositor: d.depositor,
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
            refund_to: d.refundTo,
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
    const PLATFORM: B256 = B256::repeat_byte(0x0C);

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
            b256!("e6f96c6f941ac3c40f5739ffdc56fd29f1fa4428e11a3f0640b3531574cfd06c")
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
            b256!("327d843d48b64b6d010abe26e340d8c8eb34e327aa886c7fafbb4a024eccc3f8")
        );
    }

    #[test]
    fn handle_unpublished_decodes() {
        let log = mined(
            IdentityRegistry::HandleUnpublished {
                holder: HOLDER,
                platformId: PLATFORM,
            }
            .encode_log_data(),
        );
        let (event, position) = decode_registry(&log).unwrap().expect("a known topic");
        assert!(
            matches!(
                event,
                NamesEvent::HandleUnpublished { holder, platform_id }
                    if holder == HOLDER && platform_id == PLATFORM
            ),
            "{event:?}"
        );
        assert_eq!(event.kind(), "handle_unpublished");
        assert_eq!((position.block_number, position.log_index), (7, 3));
        assert_eq!(position.block_time, 1_700_000_000);
    }

    /// The same fixed points for `HandleEscrow.sol`'s five events.
    #[test]
    fn escrow_topics_match_the_contract() {
        assert_eq!(
            HandleEscrow::Deposited::SIGNATURE_HASH,
            b256!("1ffc9b3e17a608abd621c9a1d46d5ea433d46ee04324e33c270ace681efe77a2")
        );
        assert_eq!(
            HandleEscrow::DepositedToHandle::SIGNATURE_HASH,
            b256!("97acd3a452fdde6e34045582f8f430e3d6c3bb485e62407340311cf17314fe06")
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
    fn a_deposit_decodes_with_both_of_its_parties() {
        let node = B256::repeat_byte(0x0D);
        let token = Address::repeat_byte(0xEE);
        let payer = Address::repeat_byte(0xB0);
        let log = mined(
            HandleEscrow::Deposited {
                handleNode: node,
                token,
                refundTo: payer,
                depositor: payer,
                round: U256::from(2u64),
                amount: U256::MAX,
            }
            .encode_log_data(),
        );
        let (event, _) = decode_escrow(&log).unwrap().expect("a known topic");
        assert_eq!(event.kind(), "deposited");
        assert_eq!(
            event.parties(),
            [(payer, Role::Depositor), (payer, Role::RefundTo)]
        );
    }

    /// An escrow topic means nothing from the registry, and the reverse: each
    /// contract's logs are decoded against its own events only.
    #[test]
    fn each_decoder_knows_only_its_own_contract() {
        let escrow_log = mined(
            HandleEscrow::Refunded {
                handleNode: B256::ZERO,
                token: Address::ZERO,
                refundTo: HOLDER,
                recipient: HOLDER,
                round: U256::ZERO,
                released: U256::from(1u64),
                received: U256::from(1u64),
            }
            .encode_log_data(),
        );
        assert!(decode_registry(&escrow_log).unwrap().is_none());
        let registry_log = mined(
            IdentityRegistry::HandleUnpublished {
                holder: HOLDER,
                platformId: PLATFORM,
            }
            .encode_log_data(),
        );
        assert!(decode_escrow(&registry_log).unwrap().is_none());
    }

    #[test]
    fn a_log_without_its_block_time_fails_instead_of_dating_nothing() {
        let mut log = mined(
            IdentityRegistry::HandleUnpublished {
                holder: HOLDER,
                platformId: PLATFORM,
            }
            .encode_log_data(),
        );
        log.block_timestamp = None;
        assert!(matches!(
            decode_registry(&log),
            Err(DecodeError::MissingField("block timestamp"))
        ));
    }

    /// Every kind, the largest `uint256` included, reads back from the
    /// journal row its payload writes: all of it but disclosed text, which
    /// the journal never holds.
    #[test]
    fn every_event_reads_back_from_its_journal_row() {
        let (a, b) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));
        let (n, p) = (B256::repeat_byte(0x0D), B256::repeat_byte(0x0C));
        let big = U256::MAX - U256::from(1u64);
        let x = KnownPlatform::X;
        let alice = x.handle_node("alice").unwrap();
        let text = DisclosedHandle::verify(x, alice, "Alice");
        assert!(text.is_some());
        let events = [
            NamesEvent::IdentityBound {
                holder: a,
                id_node: n,
                handle_node: p,
                platform_id: PLATFORM,
                observed_at: u64::MAX,
                ceremony_version: u16::MAX,
            },
            NamesEvent::HandlePublished {
                holder: a,
                platform_id: x.id(),
                handle_node: alice,
                handle: text.clone(),
            },
            NamesEvent::HandleRetired {
                platform_id: PLATFORM,
                handle_node: n,
                holder: a,
            },
            NamesEvent::HandleUnpublished {
                holder: a,
                platform_id: PLATFORM,
            },
            NamesEvent::ProofVerifierConfigured { verifier: b },
            NamesEvent::CeremonyBound {
                authorization_digest: p,
                holder: a,
                platform_id: PLATFORM,
                client_identifier: Bytes::from_static(b"client"),
            },
            NamesEvent::BindFeePaid {
                authorization_digest: p,
                receiver: b,
                amount: big,
            },
            NamesEvent::Escrow(EscrowEvent::Deposited {
                handle_node: n,
                token: b,
                refund_to: a,
                depositor: a,
                round: U256::from(2u64),
                amount: U256::MAX,
            }),
            NamesEvent::Escrow(EscrowEvent::DepositedToHandle {
                handle_node: alice,
                platform_id: x.id(),
                handle: text,
            }),
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                handle_node: n,
                token: b,
                depositor: a,
                holder: b,
                amount: big,
                received: big,
            }),
            NamesEvent::Escrow(EscrowEvent::Claimed {
                handle_node: n,
                token: b,
                claimer: a,
                recipient: b,
                round: U256::ZERO,
                released: big,
                received: U256::from(1u64),
            }),
            NamesEvent::Escrow(EscrowEvent::Refunded {
                handle_node: n,
                token: b,
                refund_to: a,
                recipient: b,
                round: U256::from(1u64),
                released: U256::from(5u64),
                received: U256::from(4u64),
            }),
        ];
        for event in events {
            let payload = event.payload().unwrap();
            assert!(!payload.to_string().contains("alice"), "{payload}");
            let read = NamesEvent::from_journal(event.kind(), payload)
                .unwrap_or_else(|e| panic!("{}: {e}", event.kind()));
            assert_eq!(read, event.without_text());
        }
    }

    /// Disclosed text decodes only when it hashes to the node beside it.
    #[test]
    fn a_published_handle_keeps_its_text_only_when_it_hashes_to_the_node() {
        let x = KnownPlatform::X;
        let node = x.handle_node("alice_1").unwrap();
        let published = |handle: &str| {
            let log = mined(
                IdentityRegistry::HandlePublished {
                    holder: HOLDER,
                    platformId: x.id(),
                    handleNode: node,
                    handle: handle.into(),
                }
                .encode_log_data(),
            );
            match decode_registry(&log).unwrap().expect("a known topic").0 {
                NamesEvent::HandlePublished { handle, .. } => handle,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            published("Alice_1").as_ref().map(DisclosedHandle::as_str),
            Some("alice_1")
        );
        assert_eq!(published("bob"), None);
    }

    #[test]
    fn every_role_parses_back_from_its_name() {
        for role in Role::ALL {
            assert_eq!(role.as_str().parse::<Role>().ok(), Some(role));
        }
        assert!("owner".parse::<Role>().is_err());
    }
}
