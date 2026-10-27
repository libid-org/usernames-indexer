//! A WebSocket relay between the indexer and anvil: it records the JSON-RPC
//! methods the indexer calls, and on demand cuts every connection and holds
//! new ones until it reopens, keeps the socket open but deaf, holds one
//! method's answers, delays or retracts pushed logs, or strips block times.

use std::{
    collections::{
        HashSet,
        VecDeque,
    },
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use alloy::rpc::types::Log;
use futures_util::{
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
    time::Instant,
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
struct Shared {
    /// Every method the indexer called, in order, announced as it grows.
    calls: watch::Sender<Vec<String>>,
    /// Whether logs lose their `blockTimestamp`, answered and pushed alike,
    /// as an endpoint that does not serve the field sends them.
    timeless: watch::Sender<bool>,
    /// Whether every answer and push is dropped, the socket left open and
    /// its pings answered.
    deaf: watch::Sender<bool>,
    /// The method whose answers are held until it is cleared.
    held: watch::Sender<Option<String>>,
    /// How long each pushed log is held before it is relayed.
    delay: watch::Sender<Duration>,
    /// Whether each pushed log is followed by its own removal, as a reorg
    /// takes a log back.
    retract: watch::Sender<bool>,
}

/// A JSON-RPC answer carrying logs.
#[derive(Deserialize, Serialize)]
struct LogsAnswer {
    jsonrpc: String,
    id: u64,
    result: Vec<Log>,
}

/// A pushed log.
#[derive(Clone, Deserialize, Serialize)]
struct LogPush {
    jsonrpc: String,
    method: String,
    params: PushedLog,
}

/// The subscription a log was pushed on, and the log.
#[derive(Clone, Deserialize, Serialize)]
struct PushedLog {
    subscription: String,
    result: Log,
}

/// A request's id and method.
#[derive(Deserialize)]
struct Request {
    id: u64,
    method: String,
}

/// An answer's id; a push carries none.
#[derive(Deserialize)]
struct Answer {
    id: u64,
}

impl Relay {
    /// Listen on a free local port, relaying every connection to `upstream`.
    pub async fn start(upstream: Url) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let addr = listener.local_addr().expect("the bound address");
        let shared = Arc::new(Shared {
            calls: watch::Sender::new(Vec::new()),
            timeless: watch::Sender::new(false),
            deaf: watch::Sender::new(false),
            held: watch::Sender::new(None),
            delay: watch::Sender::new(Duration::ZERO),
            retract: watch::Sender::new(false),
        });
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
                        shared.pipe(socket, upstream, open).await;
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

    /// Drop every answer and push from now on, or stop dropping them; the
    /// sockets stay open and answer pings.
    pub fn deafen(&self, deaf: bool) {
        self.shared.deaf.send_replace(deaf);
    }

    /// Hold every answer to `method` from now on, or, with `None`, relay
    /// the held answers and hold none.
    pub fn hold(&self, method: Option<&str>) {
        self.shared.held.send_replace(method.map(str::to_string));
    }

    /// Relay each pushed log `delay` after it arrives; answers pass at once.
    pub fn delay_pushes(&self, delay: Duration) {
        self.shared.delay.send_replace(delay);
    }

    /// Follow each pushed log with its removal from now on, or stop.
    pub fn retract(&self, retract: bool) {
        self.shared.retract.send_replace(retract);
    }

    /// Send logs without `blockTimestamp` from now on, or with it again.
    pub fn strip_block_times(&self, strip: bool) {
        self.shared.timeless.send_replace(strip);
    }

    /// Every method the indexer called through the relay, in order.
    pub fn calls(&self) -> Vec<String> {
        self.shared.calls.borrow().clone()
    }

    /// Return once the indexer has called `method` `times` times.
    pub async fn called(&self, method: &str, times: usize) {
        self.shared
            .calls
            .subscribe()
            .wait_for(|calls| {
                calls.iter().filter(|called| *called == method).count() >= times
            })
            .await
            .map(drop)
            .expect("the relay outlives its calls");
    }
}

impl Shared {
    /// Relay one connection both ways under the relay's switches, until
    /// either side closes or the relay is cut.
    async fn pipe(
        &self,
        socket: TcpStream,
        upstream: Url,
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
        let mut held = self.held.subscribe();
        // The ids of the requests whose answers are held while their method
        // is, and the answers held so far.
        let mut holding: HashSet<u64> = HashSet::new();
        let mut withheld: Vec<Message> = Vec::new();
        // Pushes waiting out the delay, each with when it is due.
        let mut delayed: VecDeque<(Instant, Message)> = VecDeque::new();
        loop {
            let due = delayed.front().map(|(due, _)| *due);
            tokio::select! {
                _ = async { open.wait_for(|open| !*open).await.map(drop) } => return,
                message = from_client.next() => {
                    let Some(Ok(message)) = message else { return };
                    if let Message::Text(text) = &message {
                        if let Ok(call) = serde_json::from_str::<Request>(text.as_str()) {
                            if held.borrow().as_deref() == Some(call.method.as_str()) {
                                holding.insert(call.id);
                            }
                            self.calls.send_modify(|calls| calls.push(call.method));
                        }
                    }
                    if to_server.send(message).await.is_err() {
                        return;
                    }
                }
                message = from_server.next() => {
                    let Some(Ok(mut message)) = message else { return };
                    if *self.timeless.borrow() {
                        message = Self::timeless(message);
                    }
                    if *self.deaf.borrow() {
                        continue;
                    }
                    if Self::answers(&message).is_some_and(|id| holding.contains(&id)) {
                        withheld.push(message);
                        continue;
                    }
                    let mut sent = vec![message];
                    if let Some(push) = Self::push(&sent[0]) {
                        if *self.retract.borrow() {
                            sent.push(Self::removal(push));
                        }
                        let delay = *self.delay.borrow();
                        if !delay.is_zero() {
                            let due = Instant::now() + delay;
                            delayed.extend(sent.drain(..).map(|message| (due, message)));
                        }
                    }
                    for message in sent {
                        if to_client.send(message).await.is_err() {
                            return;
                        }
                    }
                }
                () = async {
                    match due {
                        Some(due) => tokio::time::sleep_until(due).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((_, message)) = delayed.pop_front() {
                        if to_client.send(message).await.is_err() {
                            return;
                        }
                    }
                }
                changed = held.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if held.borrow().is_none() {
                        holding.clear();
                        for message in withheld.drain(..) {
                            if to_client.send(message).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    /// The id of the request `message` answers, when it is an answer.
    fn answers(message: &Message) -> Option<u64> {
        let Message::Text(text) = message else {
            return None;
        };
        serde_json::from_str::<Answer>(text.as_str())
            .ok()
            .map(|answer| answer.id)
    }

    /// The log `message` pushes, when it pushes one.
    fn push(message: &Message) -> Option<LogPush> {
        let Message::Text(text) = message else {
            return None;
        };
        serde_json::from_str::<LogPush>(text.as_str())
            .ok()
            .filter(|push| push.method == "eth_subscription")
    }

    /// The push of `push`'s log taken back by a reorg.
    fn removal(mut push: LogPush) -> Message {
        push.params.result.removed = true;
        Message::Text(
            serde_json::to_string(&push)
                .expect("a push serializes")
                .into(),
        )
    }

    /// `message` without the block times of the logs it answers or pushes.
    fn timeless(message: Message) -> Message {
        let Message::Text(text) = &message else {
            return message;
        };
        let text =
            if let Ok(mut answer) = serde_json::from_str::<LogsAnswer>(text.as_str()) {
                for log in &mut answer.result {
                    log.block_timestamp = None;
                }
                serde_json::to_string(&answer)
            } else if let Some(mut push) = Self::push(&message) {
                push.params.result.block_timestamp = None;
                serde_json::to_string(&push)
            } else {
                return message;
            };
        Message::Text(text.expect("logs serialize").into())
    }
}
