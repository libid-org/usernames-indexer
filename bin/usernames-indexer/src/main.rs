#[tokio::main]
async fn main() -> anyhow::Result<()> {
    usernames_indexer::cli::run().await
}
