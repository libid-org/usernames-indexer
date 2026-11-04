//! The JSON-RPC endpoint the loop polls.

use std::{
    fmt,
    str::FromStr,
    time::Duration,
};

use alloy::{
    providers::RootProvider,
    rpc::client::ClientBuilder,
};
use url::Url;

use super::{
    deadline::Deadline,
    retry::Retry,
    OneLine,
};

/// An `http` or `https` JSON-RPC endpoint. The loop polls over HTTP, so a
/// WebSocket or IPC address is refused where it is parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcUrl(Url);

/// An RPC URL the loop cannot poll.
#[derive(Debug, thiserror::Error)]
pub enum RpcUrlError {
    /// Not a URL at all.
    #[error(transparent)]
    Url(#[from] url::ParseError),
    /// A URL of another scheme.
    #[error("the RPC URL has to be http or https, not {0}")]
    Scheme(String),
}

impl RpcUrl {
    /// A client for this endpoint, each call failing once unanswered for
    /// `patience` and sent again after a transient error ([`Retry`]).
    pub(crate) fn provider(&self, patience: Duration) -> RootProvider {
        RootProvider::new(
            ClientBuilder::default()
                .layer(Retry(self.clone()))
                .layer(Deadline(patience))
                .http(self.0.clone()),
        )
    }

    /// `error` and its causes as one line, with this endpoint named by its
    /// setting. An HTTP client's error carries the URL it called, an
    /// endpoint's URL often holds its API key, and both the process's exit
    /// error and `/v1/status` show the line.
    pub fn redact(&self, error: &anyhow::Error) -> String {
        error.one_line().replace(self.0.as_str(), "<RPC_URL>")
    }
}

impl TryFrom<Url> for RpcUrl {
    type Error = RpcUrlError;

    fn try_from(url: Url) -> Result<Self, RpcUrlError> {
        match url.scheme() {
            "http" | "https" => Ok(Self(url)),
            other => Err(RpcUrlError::Scheme(other.to_string())),
        }
    }
}

impl FromStr for RpcUrl {
    type Err = RpcUrlError;

    fn from_str(url: &str) -> Result<Self, RpcUrlError> {
        Url::parse(url)?.try_into()
    }
}

impl fmt::Display for RpcUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            atomic::{
                AtomicUsize,
                Ordering,
            },
            Arc,
            Mutex,
        },
    };

    use alloy::{
        providers::Provider,
        rpc::json_rpc::{
            Id,
            Response,
            ResponsePayload,
        },
        transports::{
            HttpError,
            TransportErrorKind,
            TransportResult,
        },
    };
    use alloy_primitives::U64;
    use tokio::{
        io::{
            AsyncBufReadExt,
            AsyncReadExt,
            AsyncWriteExt,
            BufReader,
        },
        net::TcpListener,
    };

    use super::*;
    use crate::indexer::deadline::Unanswered;

    /// Far longer than any call here takes with all its tries.
    const PATIENCE: Duration = Duration::from_secs(20);

    /// A 503 with the body Eden's endpoint sent.
    const UNAVAILABLE: &str = "HTTP/1.1 503 Service Unavailable\r\n\
        content-length: 18\r\nconnection: close\r\n\r\nserver unavailable";

    /// A refusal of the request itself.
    const BAD_REQUEST: &str =
        "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

    /// What the endpoint does with one request.
    enum Answer {
        /// Sends these bytes back.
        Http(&'static str),
        /// Answers `eth_blockNumber` with this block.
        Block(u64),
        /// Keeps the connection open without an answer.
        Silence,
    }

    impl Answer {
        /// The bytes that answer the request with JSON-RPC id `id`, if any.
        fn to_http(&self, id: u64) -> Option<String> {
            match self {
                Self::Http(answer) => Some((*answer).to_owned()),
                Self::Block(number) => {
                    let body = serde_json::to_string(&Response {
                        id: Id::Number(id),
                        payload: ResponsePayload::<U64, ()>::Success(U64::from(*number)),
                    })
                    .expect("a response serializes");
                    Some(format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    ))
                }
                Self::Silence => None,
            }
        }
    }

    /// The part of a JSON-RPC request an answer echoes.
    #[derive(serde::Deserialize)]
    struct Request {
        id: u64,
    }

    /// An endpoint that reads each request and gives it the next of its
    /// answers, the last one again for every later request.
    struct Endpoint {
        listener: TcpListener,
        answers: Vec<Answer>,
        requests: Arc<AtomicUsize>,
    }

    impl Endpoint {
        /// What `eth_blockNumber` gets from an endpoint that answers
        /// `answers`, through a provider that waits `patience` for each try,
        /// and how many requests the endpoint read.
        async fn call(
            answers: Vec<Answer>,
            patience: Duration,
        ) -> (TransportResult<u64>, usize) {
            let endpoint = Self {
                listener: TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("a local port"),
                answers,
                requests: Arc::default(),
            };
            let rpc: RpcUrl = format!(
                "http://{}/v2/key",
                endpoint.listener.local_addr().expect("the address")
            )
            .parse()
            .expect("an http URL");
            let provider = rpc.provider(patience);
            let requests = endpoint.requests.clone();
            let server = tokio::spawn(endpoint.serve());
            let block = tokio::time::timeout(PATIENCE, provider.get_block_number())
                .await
                .expect("the call ended within its tries");
            server.abort();
            (block, requests.load(Ordering::SeqCst))
        }

        async fn serve(self) {
            loop {
                let (stream, _) = self.listener.accept().await.expect("a connection");
                let mut stream = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    let read = stream.read_line(&mut line).await.expect("a header line");
                    if read == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().expect("a length");
                        }
                    }
                }
                let mut body = vec![0; length];
                stream
                    .read_exact(&mut body)
                    .await
                    .expect("the request body");
                let Request { id } =
                    serde_json::from_slice(&body).expect("a JSON-RPC request");
                let seen = self.requests.fetch_add(1, Ordering::SeqCst);
                let answer = self.answers[seen.min(self.answers.len() - 1)].to_http(id);
                tokio::spawn(async move {
                    let Some(answer) = answer else {
                        // Holds the connection open until the test ends.
                        return std::future::pending().await;
                    };
                    stream
                        .write_all(answer.as_bytes())
                        .await
                        .expect("the answer");
                    stream.shutdown().await.expect("a closed connection");
                });
            }
        }
    }

    /// Every line a subscriber writes, kept to read back.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);

    impl Log {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("the log").clone())
                .expect("UTF-8 lines")
        }
    }

    impl io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("the log").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A refused connection's error names the URL it called, key and all, and
    /// the retry logs each failed try.
    #[tokio::test]
    async fn the_retry_log_names_no_endpoint() {
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let rpc: RpcUrl = "http://127.0.0.1:1/v2/secret".parse().expect("an http URL");
        let logging = tracing::subscriber::set_default(subscriber);
        rpc.provider(PATIENCE)
            .get_block_number()
            .await
            .expect_err("nothing listens on port 1");
        drop(logging);
        let log = log.text();
        assert!(log.contains("trying again"), "{log}");
        assert!(log.contains("<RPC_URL>"), "{log}");
        assert!(!log.contains("secret"), "{log}");
    }

    /// Eden's RPC answering one `eth_getLogs` in some 600-900 with a 503.
    #[tokio::test]
    async fn a_read_answered_after_one_503_succeeds() {
        let answers = vec![Answer::Http(UNAVAILABLE), Answer::Block(42)];
        let (block, requests) = Endpoint::call(answers, PATIENCE).await;
        assert_eq!(block.expect("the second try's block"), 42);
        assert_eq!(requests, 2);
    }

    #[tokio::test]
    async fn a_read_that_keeps_getting_503_fails_with_the_last_503() {
        let (block, requests) =
            Endpoint::call(vec![Answer::Http(UNAVAILABLE)], PATIENCE).await;
        let error = block.expect_err("an endpoint that only fails");
        let Some(TransportErrorKind::HttpError(HttpError { status, body })) =
            error.as_transport_err()
        else {
            panic!("expected the endpoint's 503, got {error:?}");
        };
        assert_eq!((*status, body.as_str()), (503, "server unavailable"));
        assert_eq!(requests, 6);
    }

    #[tokio::test]
    async fn a_read_refused_for_what_it_asks_is_sent_once() {
        let (block, requests) =
            Endpoint::call(vec![Answer::Http(BAD_REQUEST)], PATIENCE).await;
        let error = block.expect_err("a refused read");
        let Some(TransportErrorKind::HttpError(HttpError { status, .. })) =
            error.as_transport_err()
        else {
            panic!("expected an HTTP error, got {error:?}");
        };
        assert_eq!(*status, 400);
        assert_eq!(requests, 1);
    }

    /// An unanswered try costs the whole patience, and two would outlast the
    /// report; the next poll sends the read again.
    #[tokio::test]
    async fn an_unanswered_read_is_sent_once() {
        let patience = Duration::from_millis(200);
        let (block, requests) = Endpoint::call(vec![Answer::Silence], patience).await;
        let error = block.expect_err("an endpoint that never answers");
        let Some(TransportErrorKind::Custom(source)) = error.as_transport_err() else {
            panic!("expected the deadline's error, got {error:?}");
        };
        assert_eq!(source.downcast_ref(), Some(&Unanswered(patience)));
        assert_eq!(requests, 1);
    }

    #[test]
    fn only_an_http_endpoint_parses() {
        for rpc in ["https://node.example/rpc", "http://127.0.0.1:8545/"] {
            let parsed: RpcUrl = rpc.parse().expect("an http(s) URL");
            assert_eq!(parsed.to_string(), rpc);
        }
        for rpc in [
            "wss://node.example/rpc",
            "ws://127.0.0.1:8546",
            "file:///tmp/geth.ipc",
        ] {
            assert!(
                matches!(rpc.parse::<RpcUrl>(), Err(RpcUrlError::Scheme(_))),
                "{rpc}"
            );
        }
        assert!(matches!(
            "not a url".parse::<RpcUrl>(),
            Err(RpcUrlError::Url(_))
        ));
    }

    #[test]
    fn a_redacted_error_names_the_endpoint_by_its_setting() {
        let rpc: RpcUrl = "https://eth-mainnet.g.alchemy.com/v2/secret"
            .parse()
            .expect("an https URL");
        let error = anyhow::anyhow!(
            "error sending request for url (https://eth-mainnet.g.alchemy.com/v2/secret)"
        )
        .context("eth_getBlockByNumber");
        let stored = rpc.redact(&error);
        assert!(!stored.contains("secret"), "{stored}");
        assert!(stored.contains("<RPC_URL>"), "{stored}");
    }

    /// An endpoint that takes the connection and never answers fails each
    /// call once the patience runs out.
    #[tokio::test]
    async fn a_call_left_unanswered_fails_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let rpc: RpcUrl =
            format!("http://{}", listener.local_addr().expect("the address"))
                .parse()
                .expect("an http URL");
        let holding = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let provider = rpc.provider(Duration::from_millis(400));
        let answered =
            tokio::time::timeout(Duration::from_secs(10), provider.get_block_number())
                .await
                .expect("the call gave up at its deadline");
        let Err(error) = answered else {
            panic!("an endpoint that never answered answered");
        };
        assert!(error.to_string().contains("no answer within"), "{error}");
        holding.abort();
    }
}
