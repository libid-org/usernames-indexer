//! An HTTP relay between the indexer and anvil: it records the JSON-RPC
//! methods the indexer calls and when, and on demand stops answering,
//! refuses every call, or answers `eth_getLogs` without the logs' block
//! times.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};

use alloy::{
    rpc::types::Log,
    transports::http::reqwest,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{
        header::CONTENT_TYPE,
        StatusCode,
    },
    response::{
        IntoResponse,
        Response,
    },
    routing::post,
    Router,
};
use serde::{
    Deserialize,
    Serialize,
};
use tokio::{
    net::TcpListener,
    sync::watch,
};
use url::Url;
use usernames_core::indexer::RpcUrl;

/// The relay's listening end, its switches, and what it saw.
pub struct Relay {
    addr: SocketAddr,
    shared: Arc<Shared>,
}

/// What every relayed call reads and writes.
struct Shared {
    /// Where calls are relayed to.
    upstream: Url,
    client: reqwest::Client,
    /// Every call the indexer made, in order.
    calls: watch::Sender<Vec<Called>>,
    /// Whether `eth_getLogs` answers lose their `blockTimestamp`, as an
    /// endpoint that does not serve the field answers.
    timeless: watch::Sender<bool>,
    /// Whether calls go unanswered, held open until the relay hears again.
    deaf: watch::Sender<bool>,
    /// Whether every call is answered `503 Service Unavailable`.
    refused: watch::Sender<bool>,
}

/// One call the indexer made, and when the relay took it.
#[derive(Clone, Debug)]
pub struct Called {
    /// The JSON-RPC method.
    pub method: String,
    /// When the relay took the call.
    pub at: Instant,
}

/// A JSON-RPC answer carrying logs.
#[derive(Deserialize, Serialize)]
struct LogsAnswer {
    jsonrpc: String,
    id: u64,
    result: Vec<Log>,
}

/// The part of a JSON-RPC request the relay reads.
#[derive(Deserialize)]
struct Call {
    method: String,
}

impl Relay {
    /// Listen on a free local port, relaying every call to `upstream`.
    pub async fn start(upstream: Url) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let addr = listener.local_addr().expect("the bound address");
        let shared = Arc::new(Shared {
            upstream,
            client: reqwest::Client::new(),
            calls: watch::Sender::new(Vec::new()),
            timeless: watch::Sender::new(false),
            deaf: watch::Sender::new(false),
            refused: watch::Sender::new(false),
        });
        let app = Router::new()
            .route("/", post(Shared::relay))
            .with_state(shared.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { addr, shared }
    }

    /// The endpoint the indexer is given.
    pub fn url(&self) -> RpcUrl {
        format!("http://{}", self.addr)
            .parse()
            .expect("an http URL")
    }

    /// Leave every call unanswered from now on, or answer again.
    pub fn deafen(&self, deaf: bool) {
        self.shared.deaf.send_replace(deaf);
    }

    /// Answer every call `503 Service Unavailable` from now on, or relay
    /// again.
    pub fn refuse(&self, refused: bool) {
        self.shared.refused.send_replace(refused);
    }

    /// Answer `eth_getLogs` without `blockTimestamp` from now on, or with
    /// it again.
    pub fn strip_block_times(&self, strip: bool) {
        self.shared.timeless.send_replace(strip);
    }

    /// How many times the indexer has called `method` through the relay.
    pub fn count(&self, method: &str) -> usize {
        self.times(method).len()
    }

    /// When the relay took each call of `method`, in order.
    pub fn times(&self, method: &str) -> Vec<Instant> {
        self.shared
            .calls
            .borrow()
            .iter()
            .filter(|called| called.method == method)
            .map(|called| called.at)
            .collect()
    }

    /// Return once the indexer has called `method` `times` times.
    pub async fn called(&self, method: &str, times: usize) {
        self.shared
            .calls
            .subscribe()
            .wait_for(|calls| {
                calls
                    .iter()
                    .filter(|called| called.method == method)
                    .count()
                    >= times
            })
            .await
            .map(drop)
            .expect("the relay outlives its calls");
    }
}

impl Shared {
    /// Relay one call, recording its method. A deaf relay holds the call
    /// until it hears again, and the caller gives up first.
    async fn relay(State(shared): State<Arc<Self>>, call: Bytes) -> Response {
        if let Ok(Call { method }) = serde_json::from_slice(&call) {
            let at = Instant::now();
            shared
                .calls
                .send_modify(|calls| calls.push(Called { method, at }));
        }
        if *shared.refused.borrow() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if *shared.deaf.borrow() {
            shared
                .deaf
                .subscribe()
                .wait_for(|deaf| !*deaf)
                .await
                .map(drop)
                .expect("the relay outlives its calls");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let answered = shared
            .client
            .post(shared.upstream.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(call)
            .send()
            .await;
        let answer = match answered {
            Ok(answer) => answer.bytes().await,
            Err(error) => Err(error),
        };
        match answer {
            Ok(answer) if *shared.timeless.borrow() => {
                ([(CONTENT_TYPE, "application/json")], Self::timeless(answer))
                    .into_response()
            }
            Ok(answer) => ([(CONTENT_TYPE, "application/json")], answer).into_response(),
            Err(error) => (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
        }
    }

    /// `answer` without the block times of the logs it carries.
    fn timeless(answer: Bytes) -> Bytes {
        let Ok(mut logs) = serde_json::from_slice::<LogsAnswer>(&answer) else {
            return answer;
        };
        for log in &mut logs.result {
            log.block_timestamp = None;
        }
        serde_json::to_vec(&logs).expect("logs serialize").into()
    }
}
