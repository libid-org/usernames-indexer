//! Another try of a call the endpoint failed for a moment. A hosted node
//! answers `503` or drops a connection now and then; without a retry one such
//! blip ends the session and holds the cursor back a poll. A call is sent
//! again only after an error that says nothing about the request itself, and
//! the error of the last try is returned as it came, so the logs keep its
//! status and body. alloy's `RetryBackoffLayer` replaces that error with a
//! "Max retries exceeded" text. A call left unanswered is not sent again: two
//! deadlines would outlast the report, and the next poll sends it anyway.

use std::{
    task::{
        Context,
        Poll,
    },
    time::Duration,
};

use alloy::{
    rpc::json_rpc::{
        RequestPacket,
        ResponsePacket,
    },
    transports::{
        RpcError,
        TransportError,
        TransportErrorKind,
        TransportFut,
    },
};
use tower::{
    Layer,
    Service,
};

use super::rpc::RpcUrl;

/// Five more tries, 0.2 s to 3.2 s apart, about 6 s in all. Eden's 503
/// outages over 72 h of mainnet lasted up to 3.5 s; an endpoint still failing
/// after them is down for the session.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
    Duration::from_millis(3200),
];

/// The client layer that sends a call to `rpc` again after a transient error,
/// waiting the entry of [`BACKOFF`] for each try.
#[derive(Clone, Debug)]
pub(crate) struct Retry(pub(crate) RpcUrl);

/// A transport whose calls are sent again after a transient error.
#[derive(Clone, Debug)]
pub(crate) struct Retrying<S> {
    inner: S,
    rpc: RpcUrl,
}

impl<S> Layer<S> for Retry {
    type Service = Retrying<S>;

    fn layer(&self, inner: S) -> Retrying<S> {
        Retrying {
            inner,
            rpc: self.0.clone(),
        }
    }
}

impl<S> Service<RequestPacket> for Retrying<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> TransportFut<'static> {
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let rpc = self.rpc.clone();
        Box::pin(async move {
            let mut waits = BACKOFF.iter();
            loop {
                let error = match inner.call(request.clone()).await {
                    Ok(answer) => return Ok(answer),
                    Err(error) => error,
                };
                match waits.next() {
                    Some(wait) if Retry::sends_again(&error) => {
                        let error = rpc.redact(&anyhow::Error::new(error));
                        tracing::warn!(%error, ?wait, "RPC call failed; trying again");
                        tokio::time::sleep(*wait).await;
                    }
                    _ => return Err(error),
                }
            }
        })
    }
}

impl Retry {
    /// Whether the same call may pass when sent again after `error`: any 5xx,
    /// what alloy counts as a rate limit, and a request that never reached
    /// the endpoint or lost its connection.
    fn sends_again(error: &TransportError) -> bool {
        let RpcError::Transport(kind) = error else {
            return false;
        };
        match kind {
            TransportErrorKind::HttpError(http)
            | TransportErrorKind::HttpErrorWithRetryAfter { error: http, .. }
                if (500..600).contains(&http.status) =>
            {
                true
            }
            TransportErrorKind::Custom(source) => {
                match source.downcast_ref::<reqwest::Error>() {
                    Some(error) if error.is_timeout() => false,
                    Some(error) if error.is_connect() || error.is_request() => true,
                    _ => kind.is_retry_err(),
                }
            }
            _ => kind.is_retry_err(),
        }
    }
}
