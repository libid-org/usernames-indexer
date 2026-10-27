//! Where the loop learns of new blocks: an `eth_subscribe("newHeads")`
//! stream on a WebSocket, or a head read polled on a timer.

use std::{
    fmt,
    str::FromStr,
    time::Duration,
};

use alloy::{
    providers::{
        Provider,
        RootProvider,
    },
    pubsub::{
        ConnectionHandle,
        PubSubConnect,
        Subscription,
    },
    rpc::{
        client::ClientBuilder,
        types::Header,
    },
    transports::{
        ws::WsConnect,
        TransportErrorKind,
        TransportResult,
    },
};
use anyhow::Context;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::deadline::Deadline;

/// How the loop learns of new logs, configured as `subscribe` or `poll`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// An `eth_subscribe("newHeads")` stream on a WebSocket: the endpoint
    /// pushes every new head. A confirmed block is fetched when its head's
    /// bloom admits a watched contract, or when the heads before it did not
    /// arrive as one unbroken line; the cursor passes any other block
    /// without a call.
    Subscribe,
    /// `eth_getLogs` windows every poll interval, for an endpoint that serves
    /// no WebSocket.
    Poll,
}

/// A log source this build does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown log source {0:?}: expected `subscribe` or `poll`")]
pub struct UnknownLogSource(String);

impl LogSource {
    /// The name configuration spells it with.
    fn as_str(self) -> &'static str {
        match self {
            Self::Subscribe => "subscribe",
            Self::Poll => "poll",
        }
    }

    /// A connection to `rpc` for one session, each of its calls failing
    /// once unanswered for `patience`, the connection itself too. A
    /// subscription needs a WebSocket, so an `http(s)` endpoint is dialled as
    /// `ws(s)` on the same host and path, where Alchemy serves it; an
    /// endpoint whose WebSocket sits elsewhere is given as a `ws(s)` URL.
    pub(crate) async fn connect(
        self,
        rpc: &Url,
        patience: Duration,
    ) -> TransportResult<RootProvider> {
        let client = ClientBuilder::default().layer(Deadline(patience));
        let connected = async {
            match self {
                Self::Poll => client.connect(rpc.as_str()).await,
                Self::Subscribe => {
                    // One refused reconnect and alloy closes the subscription,
                    // rather than retrying ten times, three seconds apart.
                    let socket = WsConnect::new(rpc.websocket()?).with_max_retries(1);
                    client.pubsub(Unretried(socket)).await
                }
            }
        };
        let client =
            tokio::time::timeout(patience, connected)
                .await
                .map_err(|_| {
                    TransportErrorKind::custom_str(&format!(
                        "no connection within {patience:?}"
                    ))
                })??;
        Ok(RootProvider::new(client))
    }
}

impl fmt::Display for LogSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogSource {
    type Err = UnknownLogSource;

    fn from_str(name: &str) -> Result<Self, UnknownLogSource> {
        [Self::Subscribe, Self::Poll]
            .into_iter()
            .find(|source| source.as_str() == name)
            .ok_or_else(|| UnknownLogSource(name.to_string()))
    }
}

/// An RPC endpoint's WebSocket twin.
trait WebSocket {
    /// The URL as a WebSocket URL: `http` becomes `ws`, `https` becomes
    /// `wss`, and a WebSocket URL stays as it is.
    fn websocket(&self) -> TransportResult<Url>;
}

impl WebSocket for Url {
    fn websocket(&self) -> TransportResult<Url> {
        let scheme = match self.scheme() {
            "http" | "ws" => "ws",
            "https" | "wss" => "wss",
            other => {
                return Err(TransportErrorKind::custom_str(&format!(
                    "a head subscription needs an http(s) or ws(s) RPC URL, not {other}"
                )));
            }
        };
        let mut socket = self.clone();
        socket.set_scheme(scheme).map_err(|()| {
            TransportErrorKind::custom_str("the RPC URL takes no ws scheme")
        })?;
        Ok(socket)
    }
}

/// A WebSocket that stays down once it drops. alloy reconnects a dropped
/// socket and re-subscribes without telling the subscriber, which skips
/// whatever the chain emitted in between; refusing the reconnect closes the
/// subscription instead, and the loop's next session backfills from the
/// cursor.
#[derive(Clone, Debug)]
struct Unretried(WsConnect);

impl PubSubConnect for Unretried {
    fn is_local(&self) -> bool {
        self.0.is_local()
    }

    async fn connect(&self) -> TransportResult<ConnectionHandle> {
        self.0.connect().await
    }

    async fn try_reconnect(&self) -> TransportResult<ConnectionHandle> {
        Err(TransportErrorKind::backend_gone())
    }
}

/// What a session follows the chain with.
pub(crate) enum Feed {
    /// Nothing: each poll reads the head and asks for the logs itself.
    Poll,
    /// Every new head, as the endpoint pushes it.
    Subscribe(Subscription<Header>),
}

/// What ended a [`Feed::wait`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The loop was cancelled.
    Cancelled,
    /// The time waited for came.
    Tick,
    /// The endpoint pushed a head.
    Head(Box<Header>),
}

/// Heads the stream holds while the loop is busy fetching or committing.
/// Past it the stream reports a lag, which ends the session, and the next one
/// catches up from the cursor.
const STREAM_CAPACITY: usize = 1024;

impl Feed {
    /// Open what `source` follows the chain with. The stream is live on the
    /// endpoint when this returns, so every head from here on is pushed.
    pub(crate) async fn open(
        source: LogSource,
        provider: &RootProvider,
    ) -> TransportResult<Self> {
        match source {
            LogSource::Poll => Ok(Self::Poll),
            LogSource::Subscribe => Ok(Self::Subscribe(
                provider
                    .subscribe_blocks()
                    .channel_size(STREAM_CAPACITY)
                    .await?,
            )),
        }
    }

    /// Wait for cancellation, the next head, or `until`, in that order when
    /// more than one is ready: a head already pushed is never taken for the
    /// time running out. A stream that closed, overflowed or pushed
    /// something unreadable is an error: heads may be lost, and only the next
    /// session's catch-up covers them.
    pub(crate) async fn wait(
        &mut self,
        until: Instant,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Wake> {
        match self {
            Self::Poll => tokio::select! {
                _ = cancel.cancelled() => Ok(Wake::Cancelled),
                _ = tokio::time::sleep_until(until) => Ok(Wake::Tick),
            },
            Self::Subscribe(heads) => tokio::select! {
                biased;
                _ = cancel.cancelled() => Ok(Wake::Cancelled),
                head = heads.recv_result() => {
                    let head = head
                        .context("the head subscription ended")?
                        .context("the head subscription pushed something that is not a head")?;
                    Ok(Wake::Head(Box::new(head)))
                }
                _ = tokio::time::sleep_until(until) => Ok(Wake::Tick),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::B256,
        pubsub::RawSubscription,
    };
    use serde_json::value::{
        to_raw_value,
        RawValue,
    };
    use tokio::sync::broadcast;

    use std::time::Duration;

    use super::*;

    type Pushes = broadcast::Sender<Box<RawValue>>;

    /// An endpoint that takes the connection and never answers is left once
    /// the patience runs out, over either source.
    #[tokio::test]
    async fn a_connection_left_unanswered_fails_at_the_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let rpc: Url = format!("ws://{}", listener.local_addr().expect("the address"))
            .parse()
            .expect("a URL");
        let holding = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        for source in [LogSource::Poll, LogSource::Subscribe] {
            let connected = tokio::time::timeout(
                Duration::from_secs(10),
                source.connect(&rpc, Duration::from_millis(200)),
            )
            .await
            .expect("connecting gave up at its deadline");
            let Err(error) = connected else {
                panic!("{source}: connected to an endpoint that never answered");
            };
            assert!(
                error.to_string().contains("no connection within"),
                "{source}: {error}"
            );
        }
        holding.abort();
    }

    /// A head stream fed by hand: what the test sends is what it pushes.
    fn heads(capacity: usize) -> (Pushes, Feed) {
        let (pushes, rx) = broadcast::channel(capacity);
        let heads = RawSubscription {
            rx,
            local_id: B256::ZERO,
        }
        .into_typed();
        (pushes, Feed::Subscribe(heads))
    }

    /// A head at `height`, as the endpoint pushes it.
    fn head(height: u64) -> Box<RawValue> {
        let head = Header::new(alloy::consensus::Header {
            number: height,
            ..Default::default()
        });
        to_raw_value(&head).unwrap()
    }

    impl Feed {
        /// The next wake, a minute out at most.
        async fn wake(&mut self) -> anyhow::Result<Wake> {
            self.wait(
                Instant::now() + Duration::from_secs(60),
                &CancellationToken::new(),
            )
            .await
        }
    }

    #[tokio::test]
    async fn a_pushed_head_wakes_the_loop() {
        let (pushes, mut feed) = heads(8);
        pushes.send(head(12)).unwrap();
        assert!(
            matches!(feed.wake().await.unwrap(), Wake::Head(head) if head.number == 12)
        );
    }

    /// A head and the end of the interval ready at once: the head wins, so
    /// a pushed head is never taken for a quiet tick.
    #[tokio::test(start_paused = true)]
    async fn a_pushed_head_wakes_before_an_interval_that_ended_with_it() {
        let (pushes, mut feed) = heads(8);
        let cancel = CancellationToken::new();
        for height in 0..32 {
            let until = Instant::now() + Duration::from_secs(1);
            let mut wait = std::pin::pin!(feed.wait(until, &cancel));
            assert!(futures_util::poll!(&mut wait).is_pending());
            pushes.send(head(height)).unwrap();
            tokio::time::advance(Duration::from_secs(2)).await;
            let woke = wait.await.unwrap();
            assert!(
                matches!(woke, Wake::Head(ref head) if head.number == height),
                "{woke:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_stream_that_overflowed_ends_the_session() {
        let (pushes, mut feed) = heads(1);
        pushes.send(head(10)).unwrap();
        pushes.send(head(11)).unwrap();
        assert!(feed.wake().await.is_err());
    }

    #[tokio::test]
    async fn a_closed_stream_ends_the_session() {
        let (pushes, mut feed) = heads(8);
        drop(pushes);
        assert!(feed.wake().await.is_err());
    }

    #[tokio::test]
    async fn an_idle_feed_wakes_when_the_interval_ends() {
        let (_pushes, mut feed) = heads(8);
        let woke = feed
            .wait(
                Instant::now() + Duration::from_millis(20),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(woke, Wake::Tick);
    }

    #[test]
    fn a_log_source_parses_back_from_its_name() {
        for source in [LogSource::Subscribe, LogSource::Poll] {
            assert_eq!(source.to_string().parse::<LogSource>().unwrap(), source);
        }
        assert!("websocket".parse::<LogSource>().is_err());
    }

    #[test]
    fn an_http_endpoint_is_dialled_as_a_websocket_on_the_same_host_and_path() {
        let cases = [
            (
                "https://eth-mainnet.g.alchemy.com/v2/key",
                "wss://eth-mainnet.g.alchemy.com/v2/key",
            ),
            ("http://127.0.0.1:8545/", "ws://127.0.0.1:8545/"),
            (
                "wss://node.example/rpc?token=t",
                "wss://node.example/rpc?token=t",
            ),
            ("ws://127.0.0.1:8546/", "ws://127.0.0.1:8546/"),
        ];
        for (rpc, expected) in cases {
            let socket = rpc.parse::<Url>().unwrap().websocket().unwrap();
            assert_eq!(socket.as_str(), expected, "{rpc}");
        }
    }

    #[test]
    fn an_endpoint_without_a_websocket_twin_is_refused() {
        let ipc: Url = "file:///tmp/geth.ipc".parse().unwrap();
        assert!(ipc.websocket().is_err());
    }
}
