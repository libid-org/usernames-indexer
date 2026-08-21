//! Decoding `IdentityNames` logs into one typed stream.
//!
//! The enum exists so the apply path and its tests speak decoded values, not
//! `alloy` logs: a test builds a `NamesEvent` directly and never touches RPC
//! types.

use alloy::{
    primitives::{
        Address,
        B256,
    },
    rpc::types::Log,
    sol_types::SolEvent,
};
use libid_contracts::bindings::identity::IdentityNames;
use serde_json::json;

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
    /// strings.
    IdentityBound {
        owner: Address,
        id_node: B256,
        handle_node: B256,
        platform_id: B256,
        user_id: String,
        handle: String,
        observed_at: u64,
        published: bool,
        version: u32,
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
    /// A proof version gained or replaced its verifier.
    VerifierConfigured {
        platform_id: B256,
        version: u32,
        verifier: Address,
        max_future_observation: u64,
    },
    /// A proof version stopped being accepted.
    VerifierRetired { platform_id: B256, version: u32 },
    /// The version plain `bind` now uses.
    LatestVersionChanged { platform_id: B256, version: u32 },
}

impl NamesEvent {
    /// The journal's `kind` discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IdentityBound { .. } => "identity_bound",
            Self::HandleRetired { .. } => "handle_retired",
            Self::NameUnpublished { .. } => "name_unpublished",
            Self::PlatformConfigured { .. } => "platform_configured",
            Self::VerifierConfigured { .. } => "verifier_configured",
            Self::VerifierRetired { .. } => "verifier_retired",
            Self::LatestVersionChanged { .. } => "latest_version_changed",
        }
    }

    /// The journal payload. Bytes render as 0x-hex so the journal reads the
    /// way explorers print the chain.
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
                version,
            } => json!({
                "owner": owner.to_string(),
                "idNode": id_node.to_string(),
                "handleNode": handle_node.to_string(),
                "platformId": platform_id.to_string(),
                "userId": user_id,
                "handle": handle,
                "observedAt": observed_at,
                "published": published,
                "version": version,
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
            Self::VerifierConfigured {
                platform_id,
                version,
                verifier,
                max_future_observation,
            } => json!({
                "platformId": platform_id.to_string(),
                "version": version,
                "verifier": verifier.to_string(),
                "maxFutureObservation": max_future_observation,
            }),
            Self::VerifierRetired {
                platform_id,
                version,
            } => json!({
                "platformId": platform_id.to_string(),
                "version": version,
            }),
            Self::LatestVersionChanged {
                platform_id,
                version,
            } => json!({
                "platformId": platform_id.to_string(),
                "version": version,
            }),
        }
    }
}

/// Decode one log from the contract. `Ok(None)` is a topic this indexer does
/// not know — legal, because an upgraded contract may emit new events before
/// this build learns them. A log that names a known topic but fails to decode
/// is an error: skipping it would silently fork the read model from the chain.
pub fn decode(log: &Log) -> Result<Option<(NamesEvent, LogPosition)>, String> {
    let position = LogPosition {
        block_number: log
            .block_number
            .ok_or_else(|| "log without a block number".to_string())?,
        log_index: log
            .log_index
            .ok_or_else(|| "log without a log index".to_string())?,
        tx_hash: log
            .transaction_hash
            .ok_or_else(|| "log without a transaction hash".to_string())?,
    };
    let Some(&topic0) = log.topic0() else {
        return Ok(None);
    };

    let event = if topic0 == IdentityNames::IdentityBound::SIGNATURE_HASH {
        let ev = log
            .log_decode::<IdentityNames::IdentityBound>()
            .map_err(|e| format!("IdentityBound: {e}"))?;
        let d = ev.inner.data;
        NamesEvent::IdentityBound {
            owner: d.owner,
            id_node: d.idNode,
            handle_node: d.handleNode,
            platform_id: d.platformId,
            user_id: d.userId,
            handle: d.handle,
            observed_at: d.observedAt,
            published: d.published,
            version: d.version,
        }
    } else if topic0 == IdentityNames::HandleRetired::SIGNATURE_HASH {
        let ev = log
            .log_decode::<IdentityNames::HandleRetired>()
            .map_err(|e| format!("HandleRetired: {e}"))?;
        let d = ev.inner.data;
        NamesEvent::HandleRetired {
            platform_id: d.platformId,
            handle_node: d.handleNode,
            owner: d.owner,
        }
    } else if topic0 == IdentityNames::NameUnpublished::SIGNATURE_HASH {
        let ev = log
            .log_decode::<IdentityNames::NameUnpublished>()
            .map_err(|e| format!("NameUnpublished: {e}"))?;
        let d = ev.inner.data;
        NamesEvent::NameUnpublished {
            owner: d.owner,
            platform_id: d.platformId,
        }
    } else if topic0 == IdentityNames::PlatformConfigured::SIGNATURE_HASH {
        let ev = log
            .log_decode::<IdentityNames::PlatformConfigured>()
            .map_err(|e| format!("PlatformConfigured: {e}"))?;
        NamesEvent::PlatformConfigured {
            platform_id: ev.inner.data.platformId,
        }
    } else if topic0 == IdentityNames::VerifierConfigured::SIGNATURE_HASH {
        let ev = log
            .log_decode::<IdentityNames::VerifierConfigured>()
            .map_err(|e| format!("VerifierConfigured: {e}"))?;
        let d = ev.inner.data;
        NamesEvent::VerifierConfigured {
            platform_id: d.platformId,
            version: d.version,
            verifier: d.verifier,
            max_future_observation: d.maxFutureObservation,
        }
    } else if topic0 == IdentityNames::VerifierRetired::SIGNATURE_HASH {
        let ev = log
            .log_decode::<IdentityNames::VerifierRetired>()
            .map_err(|e| format!("VerifierRetired: {e}"))?;
        let d = ev.inner.data;
        NamesEvent::VerifierRetired {
            platform_id: d.platformId,
            version: d.version,
        }
    } else if topic0 == IdentityNames::LatestVersionChanged::SIGNATURE_HASH {
        let ev = log
            .log_decode::<IdentityNames::LatestVersionChanged>()
            .map_err(|e| format!("LatestVersionChanged: {e}"))?;
        let d = ev.inner.data;
        NamesEvent::LatestVersionChanged {
            platform_id: d.platformId,
            version: d.version,
        }
    } else {
        return Ok(None);
    };

    Ok(Some((event, position)))
}
