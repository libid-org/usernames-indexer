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

use super::deadline::Deadline;

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
    /// `patience`.
    pub(crate) fn provider(&self, patience: Duration) -> RootProvider {
        RootProvider::new(
            ClientBuilder::default()
                .layer(Deadline(patience))
                .http(self.0.clone()),
        )
    }

    /// `error` with this endpoint named by its setting. An HTTP client's
    /// error carries the URL it called, an endpoint's URL often holds its
    /// API key, and `/v1/status` shows the stored error to anyone.
    pub(crate) fn redact(&self, error: String) -> String {
        error.replace(self.0.as_str(), "<RPC_URL>")
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
    use alloy::providers::Provider;
    use tokio::net::TcpListener;

    use super::*;

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
        let error = "eth_blockNumber: error sending request for url \
                     (https://eth-mainnet.g.alchemy.com/v2/secret)";
        let stored = rpc.redact(error.to_string());
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
