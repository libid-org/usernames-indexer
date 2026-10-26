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
use tokio_util::sync::CancellationToken;
use url::Url;

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

    /// A connection to `rpc` for one session. A subscription needs a
    /// WebSocket, so an `http(s)` endpoint is dialled as `ws(s)` on the same
    /// host and path, where Alchemy serves it; an endpoint whose WebSocket
    /// sits elsewhere is given as a `ws(s)` URL.
    pub async fn connect(self, rpc: &Url) -> TransportResult<RootProvider> {
        match self {
            Self::Poll => RootProvider::connect(rpc.as_str()).await,
            Self::Subscribe => {
                // One refused reconnect and alloy closes the subscription,
                // rather than retrying ten times, three seconds apart.
                let socket = WsConnect::new(websocket(rpc)?).with_max_retries(1);
                let client = ClientBuilder::default().pubsub(Unretried(socket)).await?;
                Ok(RootProvider::new(client))
            }
        }
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

/// `rpc` as a WebSocket URL: `http` becomes `ws`, `https` becomes `wss`, and
/// a WebSocket URL stays as it is.
fn websocket(rpc: &Url) -> TransportResult<Url> {
    let scheme = match rpc.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => {
            return Err(TransportErrorKind::custom_str(&format!(
                "a head subscription needs an http(s) or ws(s) RPC URL, not {other}"
            )));
        }
    };
    let mut socket = rpc.clone();
    socket
        .set_scheme(scheme)
        .map_err(|()| TransportErrorKind::custom_str("the RPC URL takes no ws scheme"))?;
    Ok(socket)
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
    /// The interval ran out.
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

    /// Wait for the next head, or `interval`, or cancellation. A stream that
    /// closed, overflowed or pushed something unreadable is an error: heads
    /// may be lost, and only the next session's catch-up covers them.
    pub(crate) async fn wait(
        &mut self,
        interval: Duration,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Wake> {
        match self {
            Self::Poll => tokio::select! {
                _ = cancel.cancelled() => Ok(Wake::Cancelled),
                _ = tokio::time::sleep(interval) => Ok(Wake::Tick),
            },
            Self::Subscribe(heads) => tokio::select! {
                _ = cancel.cancelled() => Ok(Wake::Cancelled),
                head = heads.recv_result() => {
                    let head = head
                        .context("the head subscription ended")?
                        .context("the head subscription pushed something that is not a head")?;
                    Ok(Wake::Head(Box::new(head)))
                }
                _ = tokio::time::sleep(interval) => Ok(Wake::Tick),
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

    use super::*;

    type Pushes = broadcast::Sender<Box<RawValue>>;

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

    async fn wake(feed: &mut Feed) -> anyhow::Result<Wake> {
        feed.wait(Duration::from_secs(60), &CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn a_pushed_head_wakes_the_loop() {
        let (pushes, mut feed) = heads(8);
        pushes.send(head(12)).unwrap();
        assert!(
            matches!(wake(&mut feed).await.unwrap(), Wake::Head(head) if head.number == 12)
        );
    }

    #[tokio::test]
    async fn a_stream_that_overflowed_ends_the_session() {
        let (pushes, mut feed) = heads(1);
        pushes.send(head(10)).unwrap();
        pushes.send(head(11)).unwrap();
        assert!(wake(&mut feed).await.is_err());
    }

    #[tokio::test]
    async fn a_closed_stream_ends_the_session() {
        let (pushes, mut feed) = heads(8);
        drop(pushes);
        assert!(wake(&mut feed).await.is_err());
    }

    #[tokio::test]
    async fn an_idle_feed_wakes_when_the_interval_ends() {
        let (_pushes, mut feed) = heads(8);
        let woke = feed
            .wait(Duration::from_millis(20), &CancellationToken::new())
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
            let socket = websocket(&rpc.parse().unwrap()).unwrap();
            assert_eq!(socket.as_str(), expected, "{rpc}");
        }
    }

    #[test]
    fn an_endpoint_without_a_websocket_twin_is_refused() {
        assert!(websocket(&"file:///tmp/geth.ipc".parse().unwrap()).is_err());
    }
}
