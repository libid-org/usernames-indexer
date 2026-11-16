//! The escrow store's own reads.

use alloy::primitives::{
    Address,
    B256,
    U256,
};

use usernames_core::db::escrow::{
    SlotCursor,
    UnclaimedCursor,
};

#[test]
fn every_cursor_reads_back_what_it_wrote_and_nothing_else() {
    let (token, node) = (Address::repeat_byte(0xEE), B256::repeat_byte(0x0D));
    let unclaimed = UnclaimedCursor {
        token,
        held: U256::MAX,
        chain_id: 3_735_928_814,
        handle_node: node,
    };
    assert_eq!(unclaimed.to_string().parse().ok(), Some(unclaimed));
    let slot = SlotCursor {
        chain_id: 1,
        handle_node: node,
        token,
    };
    assert_eq!(slot.to_string().parse().ok(), Some(slot));
    let slot = slot.to_string();
    for garbage in ["", "1-0x0d", &format!("{slot}-1"), &format!("x{slot}")] {
        assert!(garbage.parse::<SlotCursor>().is_err(), "{garbage:?}");
    }
    assert!("1-2-3-4".parse::<UnclaimedCursor>().is_err());
}
