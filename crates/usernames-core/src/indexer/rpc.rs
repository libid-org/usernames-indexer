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
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
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
