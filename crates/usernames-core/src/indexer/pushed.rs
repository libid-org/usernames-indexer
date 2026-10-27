//! The logs a subscription pushed for blocks past the cursor, held until a
//! tick commits their blocks.

use std::collections::BTreeMap;

use alloy::rpc::types::Log;

/// Pushed logs past the cursor, keyed by their position in the chain.
#[derive(Debug, Default)]
pub(crate) struct Pushed {
    logs: BTreeMap<(u64, u64), Log>,
}

impl Pushed {
    /// Hold `log`, pushed at `block` and `index`. A log pushed with
    /// `removed` is a reorg taking back the log it names: it drops the held
    /// log at its position from the same block, and holds nothing. A log on
    /// the new branch at the same position replaces the old one.
    pub(crate) fn take(&mut self, block: u64, index: u64, log: Log) {
        let at = (block, index);
        if log.removed {
            if self
                .logs
                .get(&at)
                .is_some_and(|held| held.block_hash == log.block_hash)
            {
                self.logs.remove(&at);
            }
        } else {
            self.logs.insert(at, log);
        }
    }

    /// The held logs of blocks through `through`, in chain order, no longer
    /// held.
    pub(crate) fn settle(&mut self, through: u64) -> Vec<Log> {
        let later = match through.checked_add(1) {
            Some(next) => self.logs.split_off(&(next, 0)),
            None => BTreeMap::new(),
        };
        std::mem::replace(&mut self.logs, later)
            .into_values()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;

    use super::*;

    /// The log at `index` of `block`, on the branch `branch` names.
    fn log(block: u64, index: u64, branch: u8) -> Log {
        Log {
            block_number: Some(block),
            log_index: Some(index),
            block_hash: Some(B256::repeat_byte(branch)),
            ..Default::default()
        }
    }

    fn positions(logs: &[Log]) -> Vec<(Option<u64>, Option<u64>)> {
        logs.iter()
            .map(|log| (log.block_number, log.log_index))
            .collect()
    }

    fn take(pushed: &mut Pushed, log: Log) {
        let (block, index) = (log.block_number.unwrap(), log.log_index.unwrap());
        pushed.take(block, index, log);
    }

    #[test]
    fn held_logs_settle_in_chain_order_through_the_block_asked() {
        let mut pushed = Pushed::default();
        for (block, index) in [(12, 0), (10, 1), (10, 0), (11, 4)] {
            take(&mut pushed, log(block, index, 1));
        }
        assert_eq!(
            positions(&pushed.settle(11)),
            [
                (Some(10), Some(0)),
                (Some(10), Some(1)),
                (Some(11), Some(4))
            ]
        );
        assert!(pushed.settle(11).is_empty());
        assert_eq!(positions(&pushed.settle(u64::MAX)), [(Some(12), Some(0))]);
    }

    #[test]
    fn a_removed_log_drops_its_held_twin() {
        let mut pushed = Pushed::default();
        take(&mut pushed, log(10, 0, 1));
        take(&mut pushed, log(10, 1, 1));
        take(
            &mut pushed,
            Log {
                removed: true,
                ..log(10, 0, 1)
            },
        );
        assert_eq!(positions(&pushed.settle(10)), [(Some(10), Some(1))]);
    }

    /// A reorg pushes the new branch's log and the old one's removal in
    /// either order: the new branch's log stays.
    #[test]
    fn a_removal_leaves_the_new_branchs_log_at_its_position() {
        let mut pushed = Pushed::default();
        take(&mut pushed, log(10, 0, 1));
        take(&mut pushed, log(10, 0, 2));
        take(
            &mut pushed,
            Log {
                removed: true,
                ..log(10, 0, 1)
            },
        );
        let settled = pushed.settle(10);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].block_hash, Some(B256::repeat_byte(2)));
    }
}
