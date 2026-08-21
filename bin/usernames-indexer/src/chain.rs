//! Chain access: deployment-block discovery and windowed log fetching.

use alloy::{
    eips::BlockNumberOrTag,
    primitives::Address,
    providers::Provider,
    rpc::types::{
        Filter,
        Log,
    },
};
use tracing::{
    info,
    warn,
};

/// Binary search for the block at which a contract was deployed: the first
/// block where `eth_getCode` returns non-empty bytecode. `None` when the
/// contract has no code at `latest_block` or the RPC fails mid-search.
///
/// Cost: about log2(latest_block) RPC calls (~26 for 50M blocks).
pub async fn find_deployment_block(
    provider: &impl Provider,
    address: Address,
    latest_block: u64,
) -> Option<u64> {
    let code = provider
        .get_code_at(address)
        .block_id(BlockNumberOrTag::Number(latest_block).into())
        .await
        .ok()?;
    if code.is_empty() {
        return None;
    }

    let code_at_zero = provider
        .get_code_at(address)
        .block_id(BlockNumberOrTag::Number(0).into())
        .await
        .ok()?;
    if !code_at_zero.is_empty() {
        return Some(0);
    }

    let mut lo: u64 = 0;
    let mut hi: u64 = latest_block;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let code = provider
            .get_code_at(address)
            .block_id(BlockNumberOrTag::Number(mid).into())
            .await
            .ok()?;
        if code.is_empty() {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

/// The deployment block, or 0 when detection fails — scanning from genesis is
/// slow but never wrong.
pub async fn detect_start_block(
    provider: &impl Provider,
    address: Address,
    label: &str,
) -> u64 {
    let latest = match provider.get_block_number().await {
        Ok(n) => n,
        Err(e) => {
            warn!(%e, contract = label, "no latest block for deployment detection, falling back to 0");
            return 0;
        }
    };

    match find_deployment_block(provider, address, latest).await {
        Some(block) => {
            info!(
                block,
                contract = label,
                "detected contract deployment block"
            );
            block
        }
        None => {
            warn!(
                contract = label,
                "could not detect deployment block, falling back to 0"
            );
            0
        }
    }
}

/// One `eth_getLogs` window, inclusive on both ends. The caller sizes windows
/// with its `max_block_range`; a failure fails the window so the cursor stays
/// put and the same blocks are retried next cycle.
pub async fn fetch_logs(
    provider: &impl Provider,
    filter: &Filter,
    from: u64,
    to: u64,
) -> Result<Vec<Log>, alloy::transports::TransportError> {
    let f = filter
        .clone()
        .from_block(BlockNumberOrTag::Number(from))
        .to_block(BlockNumberOrTag::Number(to));
    provider.get_logs(&f).await
}
