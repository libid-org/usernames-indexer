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
    sol_types::SolEvent,
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
    /// A holder proved an identity. The main event: carries the plaintext
    /// id and normalized handle, so the read model needs no on-chain
    /// strings. The ceremony version is logged and never stored on chain, so
    /// this side is its only record.
    IdentityBound {
        holder: Address,
        id_node: B256,
        handle_node: B256,
        platform_id: B256,
        id: String,
        handle: String,
        observed_at: u64,
        published: bool,
        ceremony_version: u16,
    },
    /// The identity behind a handle proved a different one; the old node
    /// stops resolving but keeps its observed-at watermark.
    HandleRetired {
        platform_id: B256,
        handle_node: B256,
        holder: Address,
    },
    /// A holder withdrew its published handle.
    HandleUnpublished { holder: Address, platform_id: B256 },
    /// A platform's handle rules were configured or reconfigured.
    PlatformConfigured { platform_id: B256 },
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
        platform_id: B256,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
    },
    /// A deposit for a held node, paid straight to its holder.
    Forwarded {
        handle_node: B256,
        token: Address,
        depositor: Address,
        holder: Address,
        platform_id: B256,
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
    /// The address an identity binds to: it proved, renamed away from,
    /// withdrew, ran the ceremony of, or paid the fee of a binding, or was
    /// paid through a handle it holds.
    Holder,
    /// Held the handle until this event took it: a bind gave it to another
    /// address, or retired it from a wallet the account has left.
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
            Self::HandleRetired { .. } => "handle_retired",
            Self::HandleUnpublished { .. } => "handle_unpublished",
            Self::PlatformConfigured { .. } => "platform_configured",
            Self::ProofVerifierConfigured { .. } => "proof_verifier_configured",
            Self::CeremonyBound { .. } => "ceremony_bound",
            Self::BindFeePaid { .. } => "bind_fee_paid",
            Self::Escrow(event) => event.kind(),
        }
    }

    /// The addresses the event itself names, each with the part it plays. The
    /// apply path adds what a bind takes from addresses it does not name, the
    /// payer of a bind fee, which only the ceremony beside it names, and the
    /// `refundTo` of every deposit a claim takes. A retirement names the
    /// address whose bind retired the handle, which need not be the one that
    /// held it, so the apply path names the parties of that one too.
    pub fn parties(&self) -> Vec<(Address, Role)> {
        match self {
            Self::IdentityBound { holder, .. }
            | Self::HandleUnpublished { holder, .. }
            | Self::CeremonyBound { holder, .. } => vec![(*holder, Role::Holder)],
            Self::HandleRetired { .. } => Vec::new(),
            Self::BindFeePaid { receiver, .. } => vec![(*receiver, Role::FeeReceiver)],
            Self::PlatformConfigured { .. } | Self::ProofVerifierConfigured { .. } => {
                Vec::new()
            }
            Self::Escrow(event) => event.parties(),
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

impl EscrowEvent {
    /// The journal's `kind` discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Deposited { .. } => "deposited",
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
#[derive(Debug)]
pub enum DecodeError {
    /// The RPC returned a log without a field every mined log carries.
    MissingField(&'static str),
    /// A known topic whose payload did not decode.
    Payload {
        /// The event the topic names.
        event: &'static str,
        /// The decoder's reason.
        source: alloy::sol_types::Error,
    },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "log without a {field}"),
            Self::Payload { event, source } => write!(f, "{event}: {source}"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingField(_) => None,
            Self::Payload { source, .. } => Some(source),
        }
    }
}

/// The payload of a log whose topic named `E`. Failing here is
/// [`DecodeError::Payload`], never `Ok(None)`: the topic was recognized.
fn payload_of<E: SolEvent>(log: &Log, event: &'static str) -> Result<E, DecodeError> {
    log.log_decode::<E>()
        .map(|decoded| decoded.inner.data)
        .map_err(|source| DecodeError::Payload { event, source })
}

impl LogPosition {
    /// Where a mined log sat. Every field is one a mined log carries; the
    /// block's timestamp is one `eth_getLogs` has returned since execution-apis
    /// added it (reth, geth, anvil), and an RPC without it fails the window
    /// rather than writing a history with no time in it.
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
            id: d.id,
            handle: d.handle,
            observed_at: d.observedAt,
            published: d.published,
            ceremony_version: d.ceremonyVersion,
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
    } else if topic0 == IdentityRegistry::PlatformConfigured::SIGNATURE_HASH {
        let d: IdentityRegistry::PlatformConfigured =
            payload_of(log, "PlatformConfigured")?;
        NamesEvent::PlatformConfigured {
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
            platform_id: d.platformId,
            round: d.round,
            amount: d.amount,
        }
    } else if topic0 == HandleEscrow::Forwarded::SIGNATURE_HASH {
        let d: HandleEscrow::Forwarded = payload_of(log, "Forwarded")?;
        EscrowEvent::Forwarded {
            handle_node: d.handleNode,
            token: d.token,
            depositor: d.depositor,
            holder: d.holder,
            platform_id: d.platformId,
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
    use alloy::primitives::{
        b256,
        LogData,
    };

    use super::*;

    const HOLDER: Address = Address::repeat_byte(0xA1);
    const PLATFORM: B256 = B256::repeat_byte(0x0C);

    /// A log carrying `data`, as the RPC returns it once mined.
    fn mined(data: LogData) -> Log {
        Log {
            inner: alloy::primitives::Log {
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
            b256!("8ae08d06a548b84d8340ef35ae06cd4c34983cd87b5554e6c818b8180530950c")
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

    /// The same fixed points for `HandleEscrow.sol`'s four events.
    #[test]
    fn escrow_topics_match_the_contract() {
        assert_eq!(
            HandleEscrow::Deposited::SIGNATURE_HASH,
            b256!("71c8ac17b0d61cd3caffe11aa02503df41a1ce9009949c95908b0acce726a03d")
        );
        assert_eq!(
            HandleEscrow::Forwarded::SIGNATURE_HASH,
            b256!("03859cb562640b930fb7d5def4727f9aeb85fbed245fad32eb049ee0422dc84d")
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
                platformId: PLATFORM,
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
    /// journal row its payload writes.
    #[test]
    fn every_event_reads_back_from_its_journal_row() {
        let (a, b) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));
        let (n, p) = (B256::repeat_byte(0x0D), B256::repeat_byte(0x0C));
        let big = U256::MAX - U256::from(1u64);
        let events = [
            NamesEvent::IdentityBound {
                holder: a,
                id_node: n,
                handle_node: p,
                platform_id: PLATFORM,
                id: "1234".into(),
                handle: "alice".into(),
                observed_at: u64::MAX,
                published: true,
                ceremony_version: u16::MAX,
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
            NamesEvent::PlatformConfigured {
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
                platform_id: PLATFORM,
                round: U256::from(2u64),
                amount: U256::MAX,
            }),
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                handle_node: n,
                token: b,
                depositor: a,
                holder: b,
                platform_id: PLATFORM,
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
            let read = NamesEvent::from_journal(event.kind(), payload)
                .unwrap_or_else(|e| panic!("{}: {e}", event.kind()));
            assert_eq!(read, event);
        }
    }

    #[test]
    fn every_role_parses_back_from_its_name() {
        for role in Role::ALL {
            assert_eq!(role.as_str().parse::<Role>().ok(), Some(role));
        }
        assert!("owner".parse::<Role>().is_err());
    }
}
