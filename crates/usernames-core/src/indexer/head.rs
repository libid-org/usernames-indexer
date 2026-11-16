//! The chain head a poll reads.

use std::time::{
    Duration,
    SystemTime,
    UNIX_EPOCH,
};

use alloy::{
    consensus::BlockHeader,
    eips::BlockNumberOrTag,
    network::BlockResponse,
    providers::Provider,
};
use anyhow::Context;

/// The newest block an endpoint reports.
#[derive(Clone, Copy, Debug)]
pub struct Head {
    /// Its number.
    pub number: u64,
    /// Its timestamp, in unix seconds.
    pub time: u64,
}

/// A head older than a report may live: the endpoint answers, but its node
/// stopped following the chain or the chain stopped making blocks, and a
/// report on it would vouch for an index that cannot move. The message names
/// the head alone, so it stays the same on every poll that sees that head and
/// is stored once.
#[derive(Debug, thiserror::Error)]
#[error(
    "the head, block {number} stamped at unix time {time}, is older than STALE_AFTER: the endpoint is not following its chain, or the chain makes no blocks"
)]
pub struct StaleHead {
    /// The head's number.
    pub number: u64,
    /// The head's timestamp, in unix seconds.
    pub time: u64,
}

impl Head {
    /// The latest block `provider` reports.
    pub(crate) async fn latest(provider: &impl Provider) -> anyhow::Result<Self> {
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .context("eth_getBlockByNumber")?
            .context("eth_getBlockByNumber answered no latest block")?;
        Ok(Self {
            number: block.header().number(),
            time: block.header().timestamp(),
        })
    }

    /// This head, unless its timestamp is more than `limit` old.
    pub fn recent(self, limit: Duration) -> Result<Self, StaleHead> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        if now.saturating_sub(Duration::from_secs(self.time)) > limit {
            Err(StaleHead {
                number: self.number,
                time: self.time,
            })
        } else {
            Ok(self)
        }
    }
}
