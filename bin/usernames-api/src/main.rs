#[tokio::main]
async fn main() -> anyhow::Result<()> {
    usernames_api::run().await
}
