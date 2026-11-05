//! The retry's log, read back through a subscriber of the test's own.
//!
//! This binary holds one test. tracing caches each callsite's interest per
//! process, and a callsite another test's thread registers while this test
//! installs its subscriber can keep the interest it computed without it,
//! leaving the retry's `warn!` off and the log empty.

use std::{
    io,
    sync::{
        Arc,
        Mutex,
    },
    time::Duration,
};

use alloy::{
    primitives::Address,
    providers::Provider,
};
use usernames_core::{
    chain::Contracts,
    indexer::{
        IndexerConfig,
        RpcUrl,
    },
};

/// Every line a subscriber writes, kept to read back.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("the log").clone()).expect("UTF-8 lines")
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
    let config = IndexerConfig {
        contracts: Contracts {
            registry: Address::ZERO,
            escrow: None,
            referral_splitter: None,
        },
        confirmations: 0,
        poll_interval: Duration::from_secs(1),
        max_block_range: 1,
        start_block: None,
        stale_after: Duration::from_secs(40),
    };
    let logging = tracing::subscriber::set_default(subscriber);
    config
        .provider(&rpc)
        .get_block_number()
        .await
        .expect_err("nothing listens on port 1");
    drop(logging);
    let log = log.text();
    assert!(log.contains("trying again"), "{log}");
    assert!(log.contains("<RPC_URL>"), "{log}");
    assert!(!log.contains("secret"), "{log}");
}
