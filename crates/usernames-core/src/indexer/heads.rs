//! What the pushed heads say about the blocks past the cursor: which may
//! hold a watched contract's logs, and so must be fetched once confirmed.

use std::collections::BTreeSet;

use alloy::{
    primitives::{
        Address,
        BloomInput,
        B256,
    },
    rpc::types::Header,
};

/// The heads a session has been pushed, as far as they decide fetches.
#[derive(Debug, Default)]
pub(crate) struct Heads {
    /// The last head pushed: its height and hash.
    last: Option<(u64, B256)>,
    /// Every block up to here may hold watched logs: the heads that reached
    /// it did not arrive as one unbroken line, so their blooms do not cover
    /// every block.
    unknown_through: Option<u64>,
    /// Blocks whose head's bloom admits a watched contract.
    flagged: BTreeSet<u64>,
}

impl Heads {
    /// Take in a pushed head. A head that does not extend the last one by a
    /// block, with that block's hash as its parent — the first head, a gap,
    /// a repeat, a reorg — leaves every block up to it unknown.
    pub(crate) fn push(&mut self, head: &Header, watched: &[Address]) {
        let height = head.number;
        let extends = height
            .checked_sub(1)
            .is_some_and(|previous| self.last == Some((previous, head.parent_hash)));
        if !extends {
            self.unknown_through = self.unknown_through.max(Some(height));
        }
        let bloom = &head.logs_bloom;
        if watched
            .iter()
            .any(|contract| bloom.contains_input(BloomInput::Raw(contract.as_slice())))
        {
            self.flagged.insert(height);
        }
        self.last = Some((height, head.hash));
    }

    /// Whether blocks `from..=target` must be fetched before the cursor
    /// passes them; forgets what `target` settles.
    pub(crate) fn settle(&mut self, from: u64, target: u64) -> bool {
        let fetch = self.unknown_through.is_some_and(|unknown| unknown >= from)
            || self.flagged.range(from..=target).next().is_some();
        if self
            .unknown_through
            .is_some_and(|unknown| unknown <= target)
        {
            self.unknown_through = None;
        }
        self.flagged = self.flagged.split_off(&target.saturating_add(1));
        fetch
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{
        Bloom,
        BloomInput,
    };

    use super::*;

    const WATCHED: Address = Address::repeat_byte(0xAA);

    /// The head at `height` on the line whose block hashes are their
    /// heights, its bloom admitting `logs`.
    fn head(height: u64, logs: &[Address]) -> Header {
        let mut bloom = Bloom::default();
        for address in logs {
            bloom.accrue(BloomInput::Raw(address.as_slice()));
        }
        Header {
            hash: hash(height),
            inner: alloy::consensus::Header {
                number: height,
                parent_hash: hash(height.wrapping_sub(1)),
                logs_bloom: bloom,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn hash(height: u64) -> B256 {
        B256::left_padding_from(&height.to_be_bytes())
    }

    /// Heads `heights`, the first leaving everything up to it unknown.
    fn pushed(heights: &[(u64, &[Address])]) -> Heads {
        let mut heads = Heads::default();
        for (height, logs) in heights {
            heads.push(&head(*height, logs), &[WATCHED]);
        }
        heads
    }

    #[test]
    fn the_first_head_leaves_every_block_up_to_it_unknown() {
        let mut heads = pushed(&[(10, &[])]);
        assert!(heads.settle(4, 8));
        assert!(heads.settle(9, 10));
        assert!(!heads.settle(11, 11));
    }

    #[test]
    fn an_unbroken_line_fetches_only_the_blocks_its_blooms_admit() {
        let mut heads = pushed(&[(10, &[]), (11, &[]), (12, &[WATCHED]), (13, &[])]);
        assert!(heads.settle(1, 10), "the first head's blocks");
        assert!(!heads.settle(11, 11));
        assert!(heads.settle(12, 12));
        assert!(!heads.settle(13, 13));
    }

    #[test]
    fn another_contracts_logs_fetch_nothing() {
        let mut heads = pushed(&[(10, &[]), (11, &[Address::repeat_byte(0xBB)])]);
        heads.settle(1, 10);
        assert!(!heads.settle(11, 11));
    }

    #[test]
    fn a_gap_in_the_heads_leaves_the_blocks_up_to_it_unknown() {
        let mut heads = pushed(&[(10, &[]), (13, &[])]);
        heads.settle(1, 10);
        assert!(heads.settle(11, 11));
        assert!(heads.settle(13, 13));
        assert!(!heads.settle(14, 14));
    }

    #[test]
    fn a_head_on_another_branch_leaves_the_blocks_up_to_it_unknown() {
        let mut heads = pushed(&[(10, &[]), (11, &[])]);
        heads.settle(1, 10);
        // A reorg's new tip at 12, whose parent is not the 11 pushed before.
        let mut fork = head(12, &[]);
        fork.inner.parent_hash = B256::repeat_byte(0xF0);
        heads.push(&fork, &[WATCHED]);
        assert!(heads.settle(11, 11), "11 may have been replaced");
        assert!(heads.settle(12, 12));
    }

    #[test]
    fn a_settled_block_is_fetched_once() {
        let mut heads = pushed(&[(10, &[]), (11, &[WATCHED]), (12, &[]), (13, &[])]);
        heads.settle(1, 10);
        assert!(heads.settle(11, 11));
        assert!(!heads.settle(12, 12));
        assert!(!heads.settle(12, 13));
    }
}
