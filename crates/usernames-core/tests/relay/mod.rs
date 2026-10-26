//! A WebSocket relay between the indexer and anvil: it records the JSON-RPC
//! methods the indexer calls, and on demand cuts every connection and holds
//! new ones until it reopens, or keeps the socket open but silent.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        Mutex,
    },
};

use futures_util::{
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

/// The relay's listening end, its switches, and what it saw.
pub struct Relay {
    addr: SocketAddr,
    shared: Arc<Shared>,
    open: watch::Sender<bool>,
    /// How many connections are being relayed right now.
    live: watch::Sender<usize>,
    /// Whether subscription pushes are dropped, the socket left open.
    silent: watch::Sender<bool>,
}

/// What every relayed connection reads and writes.
#[derive(Default)]
struct Shared {
    /// Every method the indexer called, in order.
    calls: Mutex<Vec<String>>,
}

/// The part of a JSON-RPC message the relay reads: a request's method, or
/// `eth_subscription` on a push.
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
        let shared = Arc::new(Shared::default());
        let (open, _) = watch::channel(true);
        let (live, _) = watch::channel(0);
        let (silent, _) = watch::channel(false);
        let relay = Self {
            addr,
            shared: shared.clone(),
            open: open.clone(),
            live: live.clone(),
            silent: silent.clone(),
        };
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (upstream, shared, mut open, live, silent) = (
                    upstream.clone(),
                    shared.clone(),
                    open.subscribe(),
                    live.clone(),
                    silent.subscribe(),
                );
                tokio::spawn(async move {
                    // A connection made while the relay is cut waits for it to
                    // reopen, its handshake stalled as on an unreachable host.
                    let reopened = open.wait_for(|open| *open).await.is_ok();
                    if reopened {
                        live.send_modify(|live| *live += 1);
                        pipe(socket, upstream, shared, open, silent).await;
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

    /// Drop every subscription push from now on, or stop dropping them; the
    /// sockets stay open and answer calls.
    pub fn silence(&self, silent: bool) {
        self.silent.send_replace(silent);
    }

    /// Every method the indexer called through the relay, in order.
    pub fn calls(&self) -> Vec<String> {
        self.shared.calls.lock().expect("the call log").clone()
    }
}

/// Relay one connection both ways, recording each request's method and
/// dropping pushes while silenced, until either side closes or the relay is
/// cut.
async fn pipe(
    socket: TcpStream,
    upstream: Url,
    shared: Arc<Shared>,
    mut open: watch::Receiver<bool>,
    silent: watch::Receiver<bool>,
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
                        shared.calls.lock().expect("the call log").push(call.method);
                    }
                }
                if to_server.send(message).await.is_err() {
                    return;
                }
            }
            message = from_server.next() => {
                let Some(Ok(message)) = message else { return };
                let push = match &message {
                    Message::Text(text) => serde_json::from_str::<Call>(text.as_str())
                        .is_ok_and(|call| call.method == "eth_subscription"),
                    _ => false,
                };
                if push && *silent.borrow() {
                    continue;
                }
                if to_client.send(message).await.is_err() {
                    return;
                }
            }
        }
    }
}
