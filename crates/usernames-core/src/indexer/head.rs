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
pub(crate) struct Head {
    /// Its number.
    pub(crate) number: u64,
    /// Its timestamp, in unix seconds.
    time: u64,
}

/// A head older than a report may live: the endpoint answers, but its node
/// stopped following the chain, and a report on it would vouch for an index
/// that cannot move. The message names the head alone, so it stays the same
/// on every poll that sees that head and is stored once.
#[derive(Debug, thiserror::Error)]
#[error(
    "the head, block {number} stamped at unix time {time}, is older than STALE_AFTER: the endpoint is not following its chain"
)]
pub(crate) struct StaleHead {
    /// The head's number.
    number: u64,
    /// The head's timestamp, in unix seconds.
    time: u64,
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
    pub(crate) fn recent(self, limit: Duration) -> Result<Self, StaleHead> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn made(ago: Duration) -> Head {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch");
        Head {
            number: 7,
            time: now.saturating_sub(ago).as_secs(),
        }
    }

    #[test]
    fn a_head_within_the_limit_is_recent() {
        let limit = Duration::from_secs(80);
        assert_eq!(
            made(Duration::from_secs(12)).recent(limit).unwrap().number,
            7
        );
    }

    #[test]
    fn a_head_past_the_limit_is_stale() {
        let head = made(Duration::from_secs(300));
        let stale = head.recent(Duration::from_secs(80)).unwrap_err();
        assert_eq!((stale.number, stale.time), (7, head.time));
    }
}
