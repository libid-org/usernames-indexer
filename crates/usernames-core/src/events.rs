//! Decoding `IdentityNames` logs into one typed stream.
//!
//! The enum exists so the apply path and its tests speak decoded values, not
//! `alloy` logs: a test builds a `NamesEvent` directly and never touches RPC
//! types.

use alloy::{
    primitives::{
        Address,
        Bytes,
        B256,
        U256,
    },
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};
use libid_contracts::bindings::identity::IdentityNames;
use serde_json::json;

sol! {
    /// `IdentityNames.ClaimFeePaid`, verbatim from the contract. The 0.13.0
    /// bindings leave it out (libid-contracts#49 adds it); once a release
    /// carries it, this block goes and the crate's event takes its place.
    event ClaimFeePaid(bytes32 indexed authorizationDigest, address indexed receiver, uint256 amount);
}

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
}

/// One decoded `IdentityNames` event. Field names track the Solidity event
/// parameters one to one, so they carry no docs of their own — the contract's
/// natspec is the reference.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub enum NamesEvent {
    /// A wallet proved an identity. The main event: carries the plaintext
    /// userId and normalized handle, so the read model needs no on-chain
    /// strings. The ceremony version is logged and never stored on chain, so
    /// this side is its only record.
    IdentityBound {
        owner: Address,
        id_node: B256,
        handle_node: B256,
        platform_id: B256,
        user_id: String,
        handle: String,
        observed_at: u64,
        published: bool,
        ceremony_version: u16,
    },
    /// The account behind a handle proved a different one; the old node
    /// stops resolving but keeps its observed-at watermark.
    HandleRetired {
        platform_id: B256,
        handle_node: B256,
        owner: Address,
    },
    /// A wallet withdrew its displayed handle.
    NameUnpublished { owner: Address, platform_id: B256 },
    /// A platform's keyspace was configured or reconfigured.
    PlatformConfigured { platform_id: B256 },
    /// The contract was pointed at the Proof Verifier that checks every claim
    /// and holds the version set the contract itself does not.
    ProofVerifierConfigured { verifier: Address },
    /// What a ceremony carried that the binding does not keep: the client the
    /// platform authenticated, keyed by the digest that names the ceremony.
    /// Emitted beside the `IdentityBound` of the same claim.
    CeremonyBound {
        authorization_digest: B256,
        owner: Address,
        platform_id: B256,
        client_identifier: Bytes,
    },
    /// The service fee a claim's own transaction data named was paid out.
    ClaimFeePaid {
        authorization_digest: B256,
        receiver: Address,
        amount: U256,
    },
}

impl NamesEvent {
    /// The journal's `kind` discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IdentityBound { .. } => "identity_bound",
            Self::HandleRetired { .. } => "handle_retired",
            Self::NameUnpublished { .. } => "name_unpublished",
            Self::PlatformConfigured { .. } => "platform_configured",
            Self::ProofVerifierConfigured { .. } => "proof_verifier_configured",
            Self::CeremonyBound { .. } => "ceremony_bound",
            Self::ClaimFeePaid { .. } => "claim_fee_paid",
        }
    }

    /// The journal payload. Bytes render as 0x-hex so the journal reads the
    /// way explorers print the chain; a fee is a decimal string, because a
    /// `uint256` does not fit a JSON number.
    pub fn payload(&self) -> serde_json::Value {
        match self {
            Self::IdentityBound {
                owner,
                id_node,
                handle_node,
                platform_id,
                user_id,
                handle,
                observed_at,
                published,
                ceremony_version,
            } => json!({
                "owner": owner.to_string(),
                "idNode": id_node.to_string(),
                "handleNode": handle_node.to_string(),
                "platformId": platform_id.to_string(),
                "userId": user_id,
                "handle": handle,
                "observedAt": observed_at,
                "published": published,
                "ceremonyVersion": ceremony_version,
            }),
            Self::HandleRetired {
                platform_id,
                handle_node,
                owner,
            } => json!({
                "platformId": platform_id.to_string(),
                "handleNode": handle_node.to_string(),
                "owner": owner.to_string(),
            }),
            Self::NameUnpublished { owner, platform_id } => json!({
                "owner": owner.to_string(),
                "platformId": platform_id.to_string(),
            }),
            Self::PlatformConfigured { platform_id } => json!({
                "platformId": platform_id.to_string(),
            }),
            Self::ProofVerifierConfigured { verifier } => json!({
                "verifier": verifier.to_string(),
            }),
            Self::CeremonyBound {
                authorization_digest,
                owner,
                platform_id,
                client_identifier,
            } => json!({
                "authorizationDigest": authorization_digest.to_string(),
                "owner": owner.to_string(),
                "platformId": platform_id.to_string(),
                "clientIdentifier": client_identifier.to_string(),
            }),
            Self::ClaimFeePaid {
                authorization_digest,
                receiver,
                amount,
            } => json!({
                "authorizationDigest": authorization_digest.to_string(),
                "receiver": receiver.to_string(),
                "amount": amount.to_string(),
            }),
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

/// Decode one log from the contract. `Ok(None)` is a topic this indexer does
/// not know — legal, because an upgraded contract may emit new events before
/// this build learns them. A log that names a known topic but fails to decode
/// is an error: skipping it would silently fork the read model from the chain.
pub fn decode(log: &Log) -> Result<Option<(NamesEvent, LogPosition)>, DecodeError> {
    let position = LogPosition {
        block_number: log
            .block_number
            .ok_or(DecodeError::MissingField("block number"))?,
        log_index: log
            .log_index
            .ok_or(DecodeError::MissingField("log index"))?,
        tx_hash: log
            .transaction_hash
            .ok_or(DecodeError::MissingField("transaction hash"))?,
    };
    let Some(&topic0) = log.topic0() else {
        return Ok(None);
    };

    let event = if topic0 == IdentityNames::IdentityBound::SIGNATURE_HASH {
        let d: IdentityNames::IdentityBound = payload_of(log, "IdentityBound")?;
        NamesEvent::IdentityBound {
            owner: d.owner,
            id_node: d.idNode,
            handle_node: d.handleNode,
            platform_id: d.platformId,
            user_id: d.userId,
            handle: d.handle,
            observed_at: d.observedAt,
            published: d.published,
            ceremony_version: d.ceremonyVersion,
        }
    } else if topic0 == IdentityNames::HandleRetired::SIGNATURE_HASH {
        let d: IdentityNames::HandleRetired = payload_of(log, "HandleRetired")?;
        NamesEvent::HandleRetired {
            platform_id: d.platformId,
            handle_node: d.handleNode,
            owner: d.owner,
        }
    } else if topic0 == IdentityNames::NameUnpublished::SIGNATURE_HASH {
        let d: IdentityNames::NameUnpublished = payload_of(log, "NameUnpublished")?;
        NamesEvent::NameUnpublished {
            owner: d.owner,
            platform_id: d.platformId,
        }
    } else if topic0 == IdentityNames::PlatformConfigured::SIGNATURE_HASH {
        let d: IdentityNames::PlatformConfigured = payload_of(log, "PlatformConfigured")?;
        NamesEvent::PlatformConfigured {
            platform_id: d.platformId,
        }
    } else if topic0 == IdentityNames::ProofVerifierConfigured::SIGNATURE_HASH {
        let d: IdentityNames::ProofVerifierConfigured =
            payload_of(log, "ProofVerifierConfigured")?;
        NamesEvent::ProofVerifierConfigured {
            verifier: d.verifier,
        }
    } else if topic0 == IdentityNames::CeremonyBound::SIGNATURE_HASH {
        let d: IdentityNames::CeremonyBound = payload_of(log, "CeremonyBound")?;
        NamesEvent::CeremonyBound {
            authorization_digest: d.authorizationDigest,
            owner: d.owner,
            platform_id: d.platformId,
            client_identifier: d.clientIdentifier,
        }
    } else if topic0 == ClaimFeePaid::SIGNATURE_HASH {
        let d: ClaimFeePaid = payload_of(log, "ClaimFeePaid")?;
        NamesEvent::ClaimFeePaid {
            authorization_digest: d.authorizationDigest,
            receiver: d.receiver,
            amount: d.amount,
        }
    } else {
        return Ok(None);
    };

    Ok(Some((event, position)))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::b256;

    use super::*;

    /// Fixed points from `cast keccak` over the event signatures in
    /// `IdentityNames.sol` at libid-contracts v0.13.0, so a binding that
    /// drifts from the contract — the crate's, or the one declared here —
    /// fails against numbers this code never produced.
    #[test]
    fn topics_match_the_contract() {
        assert_eq!(
            IdentityNames::IdentityBound::SIGNATURE_HASH,
            b256!("8ae08d06a548b84d8340ef35ae06cd4c34983cd87b5554e6c818b8180530950c")
        );
        assert_eq!(
            IdentityNames::CeremonyBound::SIGNATURE_HASH,
            b256!("f0f0b831e902ded46acfd6caf87649edb445c2e2733992b2e79cd1420719c19c")
        );
        assert_eq!(
            ClaimFeePaid::SIGNATURE_HASH,
            b256!("86eeb882525d52c6bf9923371ee6b2753b7ca825db260239939bfa88f1c530eb")
        );
        assert_eq!(
            IdentityNames::ProofVerifierConfigured::SIGNATURE_HASH,
            b256!("01970f3cbef68f8ac95615ab5c75299e421a83a787173408d693928ed40b6574")
        );
    }
}
