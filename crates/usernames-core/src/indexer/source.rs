//! Where the loop learns of new logs: an `eth_subscribe("logs")` stream on a
//! WebSocket, or `eth_getLogs` windows polled on a timer.

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
use tokio::sync::broadcast::error::TryRecvError;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::unconfirmed::Unconfirmed;

/// How the loop learns of new logs, configured as `subscribe` or `poll`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// An `eth_subscribe("logs")` stream on a WebSocket: the endpoint pushes
    /// the watched contracts' logs as their blocks arrive, and the head is
    /// read only to confirm them and to carry the cursor past blocks without
    /// any. It needs at least one confirmation: a block's logs can still be
    /// on their way when the head first reports it.
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
    /// host and path, where Alchemy and a bare node serve it.
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

/// What a session follows the chain with between head reads.
pub(crate) enum Feed {
    /// Nothing: each poll asks for the logs itself.
    Poll,
    /// The stream, and what it pushed that is not yet confirmed.
    Subscribe {
        /// The watched contracts' logs, as the endpoint pushes them.
        logs: Subscription<Log>,
        /// Pushed logs whose blocks are not yet confirmed.
        unconfirmed: Unconfirmed,
    },
}

/// Pushed logs the stream holds while the loop is busy fetching or
/// committing. Past it the stream reports a lag, which ends the session, and
/// the next one backfills what was dropped.
const STREAM_CAPACITY: usize = 1024;

impl Feed {
    /// Open what `source` follows the chain with. A subscription is live on
    /// the endpoint when this returns, so every block from here on is pushed.
    pub(crate) async fn open(
        source: LogSource,
        provider: &RootProvider,
        filter: &Filter,
    ) -> TransportResult<Self> {
        match source {
            LogSource::Poll => Ok(Self::Poll),
            LogSource::Subscribe => Ok(Self::Subscribe {
                logs: provider
                    .subscribe_logs(filter)
                    .channel_size(STREAM_CAPACITY)
                    .await?,
                unconfirmed: Unconfirmed::default(),
            }),
        }
    }

    /// The buffer of unconfirmed logs, which only a subscription keeps.
    pub(crate) fn unconfirmed(&mut self) -> Option<&mut Unconfirmed> {
        match self {
            Self::Poll => None,
            Self::Subscribe { unconfirmed, .. } => Some(unconfirmed),
        }
    }

    /// Hold every log the stream has delivered, without waiting for more.
    /// The endpoint pushes a block's logs as the block arrives, so after a
    /// head read this holds every log of the blocks below that head.
    pub(crate) fn take_delivered(&mut self) -> anyhow::Result<()> {
        let Self::Subscribe { logs, unconfirmed } = self else {
            return Ok(());
        };
        loop {
            match logs.try_recv_result() {
                Ok(pushed) => unconfirmed.push(pushed.context(
                    "the log subscription pushed something that is not a log",
                )?)?,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(ended) => return Err(ended).context("the log subscription ended"),
            }
        }
    }

    /// Wait out `interval`, holding whatever the stream pushes meanwhile;
    /// `Ok(false)` once cancelled. A stream that closed, overflowed or pushed
    /// something unreadable is an error: logs may be lost, and only the next
    /// session's backfill finds them.
    pub(crate) async fn wait(
        &mut self,
        interval: Duration,
        cancel: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let deadline = tokio::time::sleep(interval);
        tokio::pin!(deadline);
        loop {
            match self {
                Self::Poll => tokio::select! {
                    _ = cancel.cancelled() => return Ok(false),
                    _ = &mut deadline => return Ok(true),
                },
                Self::Subscribe { logs, unconfirmed } => tokio::select! {
                    _ = cancel.cancelled() => return Ok(false),
                    _ = &mut deadline => return Ok(true),
                    pushed = logs.recv_result() => {
                        let log = pushed
                            .context("the log subscription ended")?
                            .context("the log subscription pushed something that is not a log")?;
                        unconfirmed.push(log)?;
                    }
                },
            }
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

    /// A subscription fed by hand: what the test sends is what it pushes.
    fn stream(capacity: usize) -> (broadcast::Sender<Box<RawValue>>, Feed) {
        let (pushes, rx) = broadcast::channel(capacity);
        let logs = RawSubscription {
            rx,
            local_id: B256::ZERO,
        }
        .into_typed();
        let feed = Feed::Subscribe {
            logs,
            unconfirmed: Unconfirmed::default(),
        };
        (pushes, feed)
    }

    /// A mined log at `height`, as the endpoint pushes it.
    fn pushed(height: u64) -> Box<RawValue> {
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

    /// The heights the buffer holds at or below `target`.
    fn held(feed: &mut Feed, target: u64) -> Vec<u64> {
        let unconfirmed = feed.unconfirmed().unwrap();
        unconfirmed.confirmed(target).into_keys().collect()
    }

    #[test]
    fn delivered_logs_are_held_before_the_head_decides() {
        let (pushes, mut feed) = stream(8);
        pushes.send(pushed(10)).unwrap();
        pushes.send(pushed(11)).unwrap();

        feed.take_delivered().unwrap();
        assert_eq!(held(&mut feed, 11), [10, 11]);
    }

    #[test]
    fn a_stream_that_overflowed_ends_the_session() {
        let (pushes, mut feed) = stream(1);
        pushes.send(pushed(10)).unwrap();
        pushes.send(pushed(11)).unwrap();

        assert!(feed.take_delivered().is_err());
    }

    #[test]
    fn a_closed_stream_ends_the_session() {
        let (pushes, mut feed) = stream(8);
        drop(pushes);

        assert!(feed.take_delivered().is_err());
    }

    #[tokio::test]
    async fn waiting_holds_what_the_stream_pushes_until_the_interval_ends() {
        let (pushes, mut feed) = stream(8);
        pushes.send(pushed(10)).unwrap();
        let cancel = CancellationToken::new();

        let woke = feed.wait(Duration::from_millis(50), &cancel).await.unwrap();
        assert!(woke);
        assert_eq!(held(&mut feed, 10), [10]);
    }

    #[tokio::test]
    async fn a_stream_closing_mid_wait_ends_the_session() {
        let (pushes, mut feed) = stream(8);
        drop(pushes);
        let cancel = CancellationToken::new();

        assert!(feed.wait(Duration::from_secs(60), &cancel).await.is_err());
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
