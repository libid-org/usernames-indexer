//! Pushed logs, held until their blocks are confirmed.
//!
//! A subscription pushes a log as soon as its block arrives, and a reorg can
//! replace that block before it is `CONFIRMATIONS` deep. So a pushed log waits
//! here, under the height and the hash of the block that holds it, until the
//! head is far enough past; then the canonical block at that height decides
//! which of the logs held there the chain kept.

use std::collections::{
    BTreeMap,
    HashMap,
};

use alloy::{
    primitives::B256,
    rpc::types::Log,
};

/// Logs pushed for blocks above the confirmation depth: by height, then by
/// the hash of the block that holds them, then by log index, so a log pushed
/// twice is held once.
#[derive(Debug, Default)]
pub(crate) struct Unconfirmed {
    heights: BTreeMap<u64, Forks>,
}

/// The logs held at one height, by the hash of the block that holds them. A
/// reorg can leave several blocks' logs at one height until it is confirmed
/// and the canonical hash picks one.
#[derive(Debug, Default)]
pub(crate) struct Forks(HashMap<B256, BTreeMap<u64, Log>>);

/// A pushed log that names no block, so no height can hold it. A log stream
/// pushes only mined logs; an endpoint that sends one has lost track of what
/// it streams, and only a backfill can say what it skipped.
#[derive(Debug, thiserror::Error)]
#[error("a pushed log names no block number, block hash or log index")]
pub(crate) struct UnplacedLog;

impl Unconfirmed {
    /// Hold a log. One marked `removed` belonged to a block a reorg replaced:
    /// it takes its twin out instead, and leaves its height held even when
    /// empty, so the canonical block there is read before the height commits
    /// — the replacement's logs may not have been pushed yet.
    pub(crate) fn push(&mut self, log: Log) -> Result<(), UnplacedLog> {
        let (Some(height), Some(hash), Some(index)) =
            (log.block_number, log.block_hash, log.log_index)
        else {
            return Err(UnplacedLog);
        };
        let forks = self.heights.entry(height).or_default();
        if log.removed {
            forks.remove(hash, index);
        } else {
            forks.0.entry(hash).or_default().insert(index, log);
        }
        Ok(())
    }

    /// Whether any height waits for its confirmations.
    pub(crate) fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    /// Take out every height at or below `target`, lowest first.
    pub(crate) fn confirmed(&mut self, target: u64) -> BTreeMap<u64, Forks> {
        let above = self.heights.split_off(&target.saturating_add(1));
        std::mem::replace(&mut self.heights, above)
    }
}

impl Forks {
    /// Drop one held log, and its block once it holds no other.
    fn remove(&mut self, hash: B256, index: u64) {
        if let Some(logs) = self.0.get_mut(&hash) {
            logs.remove(&index);
            if logs.is_empty() {
                self.0.remove(&hash);
            }
        }
    }

    /// The logs of the block `canonical` names, in log order; `None` when no
    /// block held here is the canonical one, and its logs must be fetched.
    pub(crate) fn into_canonical(mut self, canonical: B256) -> Option<Vec<Log>> {
        self.0
            .remove(&canonical)
            .map(|logs| logs.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{
        Address,
        Bytes,
        Log as PrimitiveLog,
        LogData,
    };

    use super::*;

    /// A mined log at `height`, in the block `fork` names, at `index`.
    fn mined(height: u64, fork: u8, index: u64) -> Log {
        Log {
            inner: PrimitiveLog {
                address: Address::repeat_byte(0xAA),
                data: LogData::new_unchecked(vec![], Bytes::new()),
            },
            block_hash: Some(B256::repeat_byte(fork)),
            block_number: Some(height),
            block_timestamp: Some(1_000 + height),
            transaction_hash: Some(B256::repeat_byte(0x77)),
            transaction_index: Some(0),
            log_index: Some(index),
            removed: false,
        }
    }

    /// The `removed` twin a reorg pushes for a log it dropped.
    fn removed(log: Log) -> Log {
        Log {
            removed: true,
            ..log
        }
    }

    /// Where each held log sits, as (height, log index), lowest first.
    fn positions(logs: &[Log]) -> Vec<(u64, u64)> {
        logs.iter()
            .map(|log| (log.block_number.unwrap(), log.log_index.unwrap()))
            .collect()
    }

    #[test]
    fn a_removed_log_takes_its_twin_out_and_leaves_its_height_to_check() {
        let mut unconfirmed = Unconfirmed::default();
        unconfirmed.push(mined(10, 0xA1, 0)).unwrap();
        unconfirmed.push(removed(mined(10, 0xA1, 0))).unwrap();

        assert!(!unconfirmed.is_empty());
        let forks = unconfirmed.confirmed(10).remove(&10).unwrap();
        assert!(forks.into_canonical(B256::repeat_byte(0xA1)).is_none());
    }

    #[test]
    fn a_removed_log_leaves_the_rest_of_its_block() {
        let mut unconfirmed = Unconfirmed::default();
        unconfirmed.push(mined(10, 0xA1, 0)).unwrap();
        unconfirmed.push(mined(10, 0xA1, 1)).unwrap();
        unconfirmed.push(removed(mined(10, 0xA1, 0))).unwrap();

        let forks = unconfirmed.confirmed(10).remove(&10).unwrap();
        let kept = forks.into_canonical(B256::repeat_byte(0xA1)).unwrap();
        assert_eq!(positions(&kept), [(10, 1)]);
    }

    #[test]
    fn a_removed_log_for_a_block_never_held_marks_its_height_to_check() {
        let mut unconfirmed = Unconfirmed::default();
        unconfirmed.push(mined(10, 0xA1, 0)).unwrap();
        unconfirmed.push(removed(mined(10, 0xB2, 0))).unwrap();
        unconfirmed.push(removed(mined(11, 0xC3, 0))).unwrap();

        let mut confirmed = unconfirmed.confirmed(11);
        assert_eq!(confirmed.keys().copied().collect::<Vec<_>>(), [10, 11]);
        let kept = confirmed
            .remove(&10)
            .unwrap()
            .into_canonical(B256::repeat_byte(0xA1))
            .unwrap();
        assert_eq!(positions(&kept), [(10, 0)]);
        let replaced = confirmed.remove(&11).unwrap();
        assert!(replaced.into_canonical(B256::repeat_byte(0xD4)).is_none());
    }

    #[test]
    fn a_log_pushed_twice_is_held_once() {
        let mut unconfirmed = Unconfirmed::default();
        unconfirmed.push(mined(10, 0xA1, 3)).unwrap();
        unconfirmed.push(mined(10, 0xA1, 3)).unwrap();

        let kept = unconfirmed
            .confirmed(10)
            .remove(&10)
            .unwrap()
            .into_canonical(B256::repeat_byte(0xA1))
            .unwrap();
        assert_eq!(positions(&kept), [(10, 3)]);
    }

    #[test]
    fn confirmed_takes_the_heights_up_to_the_target_and_keeps_the_rest() {
        let mut unconfirmed = Unconfirmed::default();
        for height in [9, 10, 11] {
            unconfirmed.push(mined(height, 0xA1, 0)).unwrap();
        }

        let confirmed = unconfirmed.confirmed(10);
        assert_eq!(confirmed.keys().copied().collect::<Vec<_>>(), [9, 10]);
        assert!(!unconfirmed.is_empty());
        assert_eq!(
            unconfirmed
                .confirmed(11)
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [11]
        );
    }

    #[test]
    fn the_canonical_hash_picks_its_block_out_of_a_reorg() {
        let mut unconfirmed = Unconfirmed::default();
        unconfirmed.push(mined(10, 0xA1, 0)).unwrap();
        unconfirmed.push(mined(10, 0xB2, 1)).unwrap();
        unconfirmed.push(mined(10, 0xB2, 0)).unwrap();

        let forks = unconfirmed.confirmed(10).remove(&10).unwrap();
        let kept = forks.into_canonical(B256::repeat_byte(0xB2)).unwrap();
        assert_eq!(positions(&kept), [(10, 0), (10, 1)]);
        assert!(kept
            .iter()
            .all(|log| log.block_hash == Some(B256::repeat_byte(0xB2))));
    }

    #[test]
    fn a_canonical_block_held_nowhere_asks_for_a_fetch() {
        let mut unconfirmed = Unconfirmed::default();
        unconfirmed.push(mined(10, 0xA1, 0)).unwrap();

        let forks = unconfirmed.confirmed(10).remove(&10).unwrap();
        assert!(forks.into_canonical(B256::repeat_byte(0xC3)).is_none());
    }

    #[test]
    fn a_log_without_a_block_is_refused() {
        let mut unconfirmed = Unconfirmed::default();
        let pending = Log {
            block_hash: None,
            ..mined(10, 0xA1, 0)
        };

        assert!(unconfirmed.push(pending).is_err());
        assert!(unconfirmed.is_empty());
    }
}
