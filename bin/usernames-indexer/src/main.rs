//! Indexes the chain the environment configures until stopped.

use clap::Parser;
use tracing_subscriber::EnvFilter;
use usernames_indexer::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    Config::parse().run().await
}
