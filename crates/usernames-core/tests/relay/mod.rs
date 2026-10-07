//! A WebSocket relay between the indexer and anvil: it records the JSON-RPC
//! methods the indexer calls, and on demand cuts every connection, holds new
//! ones until it reopens, or answers one head read from behind the chain.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        Mutex,
    },
};

use alloy::primitives::U64;
use futures::{
    SinkExt,
    StreamExt,
};
use serde::{
    Deserialize,
    Serialize,
};
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

/// The relay's listening end, its switches, and what it saw.
pub struct Relay {
    addr: SocketAddr,
    shared: Arc<Shared>,
    open: watch::Sender<bool>,
    /// How many connections are being relayed right now.
    live: watch::Sender<usize>,
}

/// What every relayed connection reads and writes.
#[derive(Default)]
struct Shared {
    /// Every method the indexer called, in order.
    calls: Mutex<Vec<String>>,
    /// Blocks to take off the next `eth_blockNumber` answer.
    head_lag: Mutex<Option<u64>>,
}

/// The part of a JSON-RPC request the relay reads.
#[derive(Deserialize)]
struct Call {
    id: u64,
    method: String,
}

/// A JSON-RPC answer carrying a quantity, as `eth_blockNumber` answers.
#[derive(Deserialize, Serialize)]
struct Quantity {
    jsonrpc: String,
    id: u64,
    result: U64,
}

impl Relay {
    /// Listen on a free local port, relaying every connection to `upstream`.
    pub async fn start(upstream: Url) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let addr = listener.local_addr().expect("the bound address");
        let shared = Arc::new(Shared::default());
        let (open, _) = watch::channel(true);
        let (live, _) = watch::channel(0);
        let relay = Self {
            addr,
            shared: shared.clone(),
            open: open.clone(),
            live: live.clone(),
        };
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (upstream, shared, mut open, live) = (
                    upstream.clone(),
                    shared.clone(),
                    open.subscribe(),
                    live.clone(),
                );
                tokio::spawn(async move {
                    // A connection made while the relay is cut waits for it to
                    // reopen, its handshake stalled as on an unreachable host.
                    let reopened = open.wait_for(|open| *open).await.is_ok();
                    if reopened {
                        live.send_modify(|live| *live += 1);
                        pipe(socket, upstream, shared, open).await;
                        live.send_modify(|live| *live -= 1);
                    }
                });
            }
        });
        relay
    }

    /// The endpoint the indexer is given: `http`, so the indexer derives the
    /// WebSocket URL it dials.
    pub fn url(&self) -> Url {
        format!("http://{}", self.addr).parse().expect("a URL")
    }

    /// Drop every relayed connection and hold new ones until reopened.
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

    /// Relay again, the held connections first.
    pub fn reopen(&self) {
        self.open.send_replace(true);
    }

    /// Answer the next `eth_blockNumber` `blocks` short of the chain head, as
    /// a node behind the rest of a balanced endpoint would.
    pub fn lag_next_head(&self, blocks: u64) {
        *self.shared.head_lag.lock().expect("the head lag") = Some(blocks);
    }

    /// Every method the indexer called through the relay, in order.
    pub fn calls(&self) -> Vec<String> {
        self.shared.calls.lock().expect("the call log").clone()
    }
}

/// Relay one connection both ways, recording each request's method and
/// lagging the head read it was told to, until either side closes or the
/// relay is cut.
async fn pipe(
    socket: TcpStream,
    upstream: Url,
    shared: Arc<Shared>,
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
    // Request ids whose answers are to be lagged, and by how much.
    let mut lagged = HashMap::new();
    loop {
        tokio::select! {
            _ = async { open.wait_for(|open| !*open).await.map(drop) } => return,
            message = from_client.next() => {
                let Some(Ok(message)) = message else { return };
                if let Message::Text(text) = &message {
                    if let Ok(call) = serde_json::from_str::<Call>(text.as_str()) {
                        let lag = shared.head_lag.lock().expect("the head lag").take_if(|_| call.method == "eth_blockNumber");
                        if let Some(blocks) = lag {
                            lagged.insert(call.id, blocks);
                        }
                        shared.calls.lock().expect("the call log").push(call.method);
                    }
                }
                if to_server.send(message).await.is_err() {
                    return;
                }
            }
            message = from_server.next() => {
                let Some(Ok(message)) = message else { return };
                let message = match &message {
                    Message::Text(text) => match serde_json::from_str::<Quantity>(text.as_str()) {
                        Ok(answer) if lagged.contains_key(&answer.id) => {
                            let blocks = lagged.remove(&answer.id).unwrap_or_default();
                            let behind = Quantity {
                                result: answer.result.saturating_sub(U64::from(blocks)),
                                ..answer
                            };
                            Message::text(serde_json::to_string(&behind).expect("an answer"))
                        }
                        _ => message,
                    },
                    _ => message,
                };
                if to_client.send(message).await.is_err() {
                    return;
                }
            }
        }
    }
}
