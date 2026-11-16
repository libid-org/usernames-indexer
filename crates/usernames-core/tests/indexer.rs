//! The indexing loop's own rules, apart from a chain.

use usernames_core::indexer::*;

/// An error that prints its cause in its own message and returns it as
/// its source, as sqlx's and alloy's do.
#[derive(Debug, thiserror::Error)]
#[error("answer lost: {0}")]
struct Repeats(#[source] std::io::Error);

#[test]
fn a_stored_error_names_each_cause_once() {
    let error = anyhow::Error::new(Repeats(std::io::Error::other("socket closed")))
        .context("eth_blockNumber");
    assert_eq!(
        error.one_line(),
        "eth_blockNumber: answer lost: socket closed"
    );
}
