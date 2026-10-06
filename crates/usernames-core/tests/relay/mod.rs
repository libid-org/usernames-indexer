//! A WebSocket relay between the indexer and anvil: it records the JSON-RPC
//! methods the indexer calls, and on demand cuts every connection and
//! refuses new ones until it reopens — a dropped socket, when a test wants
//! one.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        Mutex,
    },
};

use futures::{
    SinkExt,
    StreamExt,
};
use serde::Deserialize;
use tokio::{
    net::{
        TcpListener,
        TcpStream,
    },
    sync::watch,
};
use tokio_tungstenite::{
    accept_async,
    connect_async,
    tungstenite::Message,
};
use url::Url;

/// The relay's listening end and what it saw.
pub struct Relay {
    addr: SocketAddr,
    calls: Arc<Mutex<Vec<String>>>,
    open: watch::Sender<bool>,
    /// How many connections are being relayed right now.
    live: watch::Sender<usize>,
}

/// The part of a JSON-RPC request the relay records.
#[derive(Deserialize)]
struct Call {
    method: String,
}

impl Relay {
    /// Listen on a free local port, relaying every connection to `upstream`.
    pub async fn start(upstream: Url) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let addr = listener.local_addr().expect("the bound address");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (open, _) = watch::channel(true);
        let (live, _) = watch::channel(0);
        let relay = Self {
            addr,
            calls: calls.clone(),
            open: open.clone(),
            live: live.clone(),
        };
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                if *open.borrow() {
                    let (upstream, calls, open) =
                        (upstream.clone(), calls.clone(), open.subscribe());
                    live.send_modify(|live| *live += 1);
                    let live = live.clone();
                    tokio::spawn(async move {
                        pipe(socket, upstream, calls, open).await;
                        live.send_modify(|live| *live -= 1);
                    });
                }
            }
        });
        relay
    }

    /// The endpoint the indexer is given: `http`, so the indexer derives the
    /// WebSocket URL it dials.
    pub fn url(&self) -> Url {
        format!("http://{}", self.addr).parse().expect("a URL")
    }

    /// Drop every relayed connection, and refuse new ones until reopened.
    /// Returns once the last relayed connection is closed.
    pub async fn cut(&self) {
        self.open.send_replace(false);
        self.live
            .subscribe()
            .wait_for(|live| *live == 0)
            .await
            .map(drop)
            .expect("the relay outlives its connections");
    }

    /// Relay new connections again.
    pub fn reopen(&self) {
        self.open.send_replace(true);
    }

    /// Every method the indexer called through the relay, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("the call log").clone()
    }
}

/// Relay one connection both ways, recording each request's method, until
/// either side closes or the relay is cut.
async fn pipe(
    socket: TcpStream,
    upstream: Url,
    calls: Arc<Mutex<Vec<String>>>,
    mut open: watch::Receiver<bool>,
) {
    let Ok(client) = accept_async(socket).await else {
        return;
    };
    let Ok((server, _)) = connect_async(upstream.as_str()).await else {
        return;
    };
    let (mut to_client, mut from_client) = client.split();
    let (mut to_server, mut from_server) = server.split();
    loop {
        tokio::select! {
            _ = async { open.wait_for(|open| !*open).await.map(drop) } => return,
            message = from_client.next() => {
                let Some(Ok(message)) = message else { return };
                if let Message::Text(text) = &message {
                    if let Ok(call) = serde_json::from_str::<Call>(text.as_str()) {
                        calls.lock().expect("the call log").push(call.method);
                    }
                }
                if to_server.send(message).await.is_err() {
                    return;
                }
            }
            message = from_server.next() => {
                let Some(Ok(message)) = message else { return };
                if to_client.send(message).await.is_err() {
                    return;
                }
            }
        }
    }
}
