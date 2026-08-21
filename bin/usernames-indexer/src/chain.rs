//! Chain access: deployment-block discovery and windowed log fetching.

use alloy::{
    eips::BlockNumberOrTag,
    primitives::Address,
    providers::Provider,
    rpc::types::{
        Filter,
        Log,
    },
    transports::TransportError,
};

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
