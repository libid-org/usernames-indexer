//! Chain access: deployment-block discovery, windowed log fetching, and the
//! one contract read the indexer makes.

use alloy::{
    contract,
    eips::BlockNumberOrTag,
    primitives::Address,
    providers::Provider,
    rpc::types::{
        Filter,
        Log,
    },
    transports::TransportError,
};
use libid_contracts::bindings::escrow::HandleEscrow;

/// Binary search for the block at which a contract was deployed: the first
/// block where `eth_getCode` returns non-empty bytecode. `Ok(None)` means the
/// contract has no code at `latest_block` — absence is an answer. An RPC
/// failure is an `Err`, kept apart so a transient flake cannot masquerade as
/// "not deployed" and end up cached as if it were a fact.
///
/// Cost: about log2(latest_block) RPC calls (~26 for 50M blocks).
pub async fn find_deployment_block(
    provider: &impl Provider,
    address: Address,
    latest_block: u64,
) -> Result<Option<u64>, TransportError> {
    let code = provider
        .get_code_at(address)
        .block_id(BlockNumberOrTag::Number(latest_block).into())
        .await?;
    if code.is_empty() {
        return Ok(None);
    }

    let code_at_zero = provider
        .get_code_at(address)
        .block_id(BlockNumberOrTag::Number(0).into())
        .await?;
    if !code_at_zero.is_empty() {
        return Ok(Some(0));
    }

    let mut lo: u64 = 0;
    let mut hi: u64 = latest_block;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let code = provider
            .get_code_at(address)
            .block_id(BlockNumberOrTag::Number(mid).into())
            .await?;
        if code.is_empty() {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(Some(lo))
}

/// One `eth_getLogs` window, inclusive on both ends. The caller sizes windows
/// with its `max_block_range`; a failure fails the window so the cursor stays
/// put and the same blocks are retried next cycle.
pub async fn fetch_logs(
    provider: &impl Provider,
    filter: &Filter,
    from: u64,
    to: u64,
) -> Result<Vec<Log>, TransportError> {
    let f = filter
        .clone()
        .from_block(BlockNumberOrTag::Number(from))
        .to_block(BlockNumberOrTag::Number(to));
    provider.get_logs(&f).await
}

/// Why an escrow is refused beside the registry an indexer follows.
#[derive(Debug, thiserror::Error)]
pub enum EscrowRefused {
    /// The escrow's `registry()` could not be read.
    #[error("escrow {escrow}: registry(): {source}")]
    Unread {
        /// The escrow asked.
        escrow: Address,
        /// The call's failure.
        #[source]
        source: contract::Error,
    },
    /// The escrow pays the holders of another registry.
    #[error(
        "escrow {escrow} resolves holders through registry {registry}, not {indexed}"
    )]
    OtherRegistry {
        /// The escrow asked.
        escrow: Address,
        /// The registry it resolves holders through.
        registry: Address,
        /// The registry the indexer follows.
        indexed: Address,
    },
}

/// Refuse an escrow that does not resolve holders through `registry`. An
/// escrow keeps its registry for life, so an indexer checks once, at startup,
/// before it touches a row.
pub async fn check_escrow(
    provider: &impl Provider,
    escrow: Address,
    registry: Address,
) -> Result<(), EscrowRefused> {
    let resolves = HandleEscrow::new(escrow, provider)
        .registry()
        .call()
        .await
        .map_err(|source| EscrowRefused::Unread { escrow, source })?;
    if resolves == registry {
        Ok(())
    } else {
        Err(EscrowRefused::OtherRegistry {
            escrow,
            registry: resolves,
            indexed: registry,
        })
    }
}
