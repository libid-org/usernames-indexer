//! History pages: the cursor a page hands out.

use usernames_core::db::history::HistoryCursor;

#[test]
fn a_cursor_reads_back_what_it_wrote_and_nothing_else() {
    let cursor = HistoryCursor {
        block_time: 1_790_000_000,
        chain_id: 3_735_928_814,
        block_number: 274_217_956,
        log_index: 2,
    };
    assert_eq!(
        cursor.to_string().parse::<HistoryCursor>().ok(),
        Some(cursor)
    );
    for garbage in ["", "1-2-3", "1-2-3-4-5", "a-2-3-4", "1--2-3", "1-2-3-"] {
        assert!(garbage.parse::<HistoryCursor>().is_err(), "{garbage:?}");
    }
}
