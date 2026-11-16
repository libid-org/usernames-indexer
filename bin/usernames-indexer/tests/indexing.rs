//! How the indexer stops its loops.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use usernames_indexer::Indexing;

/// A loop that never reaches its next cancellation check is aborted
/// once its time is up, so the process can exit.
#[tokio::test]
async fn a_loop_that_does_not_stop_is_aborted() {
    let mut indexing = Indexing {
        cancel: CancellationToken::new(),
        task: tokio::spawn(std::future::pending()),
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        indexing.stop(Duration::from_millis(50)),
    )
    .await
    .expect("stop returns");
    let ended = tokio::time::timeout(Duration::from_secs(1), indexing.task)
        .await
        .expect("the loop ended");
    assert!(ended.unwrap_err().is_cancelled());
}
