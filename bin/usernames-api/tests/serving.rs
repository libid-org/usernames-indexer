//! How the server answers a request it cannot finish.

use std::{
    sync::Arc,
    time::Duration,
};

use axum::{
    routing::get,
    Router,
};
use tokio::{
    io::AsyncWriteExt,
    net::{
        TcpListener,
        TcpStream,
    },
    sync::Notify,
};

use usernames_api::Serving;

/// A request that is never answered is dropped once its time is up, so
/// the process can exit.
#[tokio::test]
async fn a_request_that_is_never_answered_is_dropped_in_time() {
    let entered = Arc::new(Notify::new());
    let app = Router::new().route(
        "/never",
        get({
            let entered = entered.clone();
            move || async move {
                entered.notify_one();
                std::future::pending::<()>().await
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let addr = listener.local_addr().expect("the port");
    let mut serving = Serving::start(listener, app);
    let mut client = TcpStream::connect(addr).await.expect("a connection");
    client
        .write_all(b"GET /never HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .expect("the request");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the request reached its handler");

    tokio::time::timeout(
        Duration::from_secs(5),
        serving.stop(Duration::from_millis(50)),
    )
    .await
    .expect("stop returns");
    let ended = tokio::time::timeout(Duration::from_secs(1), serving.task)
        .await
        .expect("the server ended");
    assert!(ended.unwrap_err().is_cancelled());
}
