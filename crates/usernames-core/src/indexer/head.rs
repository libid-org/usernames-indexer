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
/// that cannot move.
#[derive(Debug, thiserror::Error)]
#[error(
    "the head, block {number}, is {}s old, past STALE_AFTER: the endpoint is not following its chain",
    age.as_secs()
)]
pub(crate) struct StaleHead {
    /// The head's number.
    number: u64,
    /// How old its timestamp is by this host's clock.
    age: Duration,
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
        let age = now.saturating_sub(Duration::from_secs(self.time));
        if age > limit {
            Err(StaleHead {
                number: self.number,
                age,
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
        let stale = made(Duration::from_secs(300))
            .recent(Duration::from_secs(80))
            .unwrap_err();
        assert_eq!(stale.number, 7);
        assert!(stale.age >= Duration::from_secs(300), "{stale}");
    }
}
