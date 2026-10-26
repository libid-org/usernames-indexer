//! Where the loop learns of new blocks and logs: `eth_subscribe` streams of
//! heads and of the watched contracts' logs on a WebSocket, or `eth_getLogs`
//! windows polled on a timer.

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
        types::{
            Filter,
            Header,
            Log,
        },
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
    /// `eth_subscribe` streams on a WebSocket: the endpoint pushes every new
    /// head and the watched contracts' logs as their blocks arrive. A block
    /// is fetched once confirmed when a log was pushed for it; the cursor
    /// passes any other block without a call. It needs at least one
    /// confirmation: a block's logs can still be on their way when its
    /// successor's head arrives.
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
                "a log subscription needs an http(s) or ws(s) RPC URL, not {other}"
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
    /// The streams the endpoint pushes.
    Subscribe {
        /// Every new head.
        heads: Subscription<Header>,
        /// The watched contracts' logs.
        logs: Subscription<Log>,
    },
}

/// What ended a [`Feed::wait`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The loop was cancelled.
    Cancelled,
    /// The interval ran out.
    Tick,
    /// The endpoint pushed a head at this height.
    Head(u64),
    /// The endpoint pushed a log from this block, or withdrew one a reorg
    /// removed.
    Log(u64),
}

/// Pushes a stream holds while the loop is busy fetching or committing. Past
/// it the stream reports a lag, which ends the session, and the next one
/// backfills from the cursor.
const STREAM_CAPACITY: usize = 1024;

impl Feed {
    /// Open what `source` follows the chain with. The streams are live on the
    /// endpoint when this returns, so every block from here on is pushed.
    pub(crate) async fn open(
        source: LogSource,
        provider: &RootProvider,
        filter: &Filter,
    ) -> TransportResult<Self> {
        match source {
            LogSource::Poll => Ok(Self::Poll),
            LogSource::Subscribe => {
                let logs = provider
                    .subscribe_logs(filter)
                    .channel_size(STREAM_CAPACITY)
                    .await?;
                let heads = provider
                    .subscribe_blocks()
                    .channel_size(STREAM_CAPACITY)
                    .await?;
                Ok(Self::Subscribe { heads, logs })
            }
        }
    }

    /// Wait for the next push, or `interval`, or cancellation. A stream that
    /// closed, overflowed or pushed something unreadable is an error: logs
    /// may be lost, and only the next session's catch-up finds them.
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
            // In this order: a block's logs are pushed before the head that
            // confirms it, and a head taken first would let the cursor pass a
            // block whose logs wait in the other stream.
            Self::Subscribe { heads, logs } => tokio::select! {
                biased;
                _ = cancel.cancelled() => Ok(Wake::Cancelled),
                log = logs.recv_result() => {
                    let log = log
                        .context("the log subscription ended")?
                        .context("the log subscription pushed something that is not a log")?;
                    let block = log
                        .block_number
                        .context("the log subscription pushed a log without its block")?;
                    Ok(Wake::Log(block))
                }
                head = heads.recv_result() => {
                    let head = head
                        .context("the head subscription ended")?
                        .context("the head subscription pushed something that is not a head")?;
                    Ok(Wake::Head(head.number))
                }
                _ = tokio::time::sleep(interval) => Ok(Wake::Tick),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{
            Address,
            Bytes,
            Log as PrimitiveLog,
            LogData,
            B256,
        },
        pubsub::RawSubscription,
    };
    use serde_json::value::{
        to_raw_value,
        RawValue,
    };
    use tokio::sync::broadcast;

    use super::*;

    type Pushes = broadcast::Sender<Box<RawValue>>;

    /// A subscription fed by hand: what the test sends is what it pushes.
    fn stream<T>(capacity: usize) -> (Pushes, Subscription<T>) {
        let (pushes, rx) = broadcast::channel(capacity);
        let subscription = RawSubscription {
            rx,
            local_id: B256::ZERO,
        }
        .into_typed();
        (pushes, subscription)
    }

    /// Head and log streams fed by hand.
    fn streams(capacity: usize) -> (Pushes, Pushes, Feed) {
        let (heads, head_stream) = stream(capacity);
        let (logs, log_stream) = stream(capacity);
        let feed = Feed::Subscribe {
            heads: head_stream,
            logs: log_stream,
        };
        (heads, logs, feed)
    }

    /// A head at `height`, as the endpoint pushes it.
    fn head(height: u64) -> Box<RawValue> {
        let head = Header::new(alloy::consensus::Header {
            number: height,
            ..Default::default()
        });
        to_raw_value(&head).unwrap()
    }

    /// A mined log at `height`, as the endpoint pushes it.
    fn log(height: u64) -> Box<RawValue> {
        let log = Log {
            inner: PrimitiveLog {
                address: Address::repeat_byte(0xAA),
                data: LogData::new_unchecked(vec![], Bytes::new()),
            },
            block_hash: Some(B256::repeat_byte(0xA1)),
            block_number: Some(height),
            block_timestamp: Some(1_000 + height),
            transaction_hash: Some(B256::repeat_byte(0x77)),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        };
        to_raw_value(&log).unwrap()
    }

    async fn wake(feed: &mut Feed) -> anyhow::Result<Wake> {
        feed.wait(Duration::from_secs(60), &CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn a_pushed_head_or_log_wakes_the_loop_with_its_block() {
        let (heads, logs, mut feed) = streams(8);
        heads.send(head(12)).unwrap();
        assert_eq!(wake(&mut feed).await.unwrap(), Wake::Head(12));
        logs.send(log(11)).unwrap();
        assert_eq!(wake(&mut feed).await.unwrap(), Wake::Log(11));
    }

    #[tokio::test]
    async fn a_log_waiting_beside_a_later_head_wakes_first() {
        let (heads, logs, mut feed) = streams(8);
        heads.send(head(13)).unwrap();
        logs.send(log(11)).unwrap();
        assert_eq!(wake(&mut feed).await.unwrap(), Wake::Log(11));
        assert_eq!(wake(&mut feed).await.unwrap(), Wake::Head(13));
    }

    #[tokio::test]
    async fn a_stream_that_overflowed_ends_the_session() {
        let (_heads, logs, mut feed) = streams(1);
        logs.send(log(10)).unwrap();
        logs.send(log(11)).unwrap();
        assert!(wake(&mut feed).await.is_err());
    }

    #[tokio::test]
    async fn a_closed_stream_ends_the_session() {
        let (heads, _logs, mut feed) = streams(8);
        drop(heads);
        assert!(wake(&mut feed).await.is_err());
    }

    #[tokio::test]
    async fn an_idle_feed_wakes_when_the_interval_ends() {
        let (_heads, _logs, mut feed) = streams(8);
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
