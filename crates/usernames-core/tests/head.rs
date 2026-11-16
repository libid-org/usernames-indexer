//! The head an indexer reads up to.

use std::time::{
    Duration,
    SystemTime,
    UNIX_EPOCH,
};

use usernames_core::indexer::head::*;

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
