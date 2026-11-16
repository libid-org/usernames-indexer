//! What a failed startup reports.

use clap::Parser;

use usernames_indexer::Config;

/// An endpoint's URL often holds its API key, and an HTTP client's error
/// names the URL it called: a startup that fails on its endpoint names
/// the endpoint by its setting.
#[tokio::test]
async fn a_failed_startup_names_the_endpoint_by_its_setting() {
    let config = Config::try_parse_from([
        "usernames-indexer",
        "--database-url",
        "postgres://localhost/usernames",
        "--rpc-url",
        "http://127.0.0.1:1/v2/api-key-marker",
        "--identity-registry-address",
        "0x0000000000000000000000000000000000000001",
        "--chain-names",
        "eden",
    ])
    .expect("parses");
    let Err(refused) = config.start().await else {
        panic!("nothing answers on port 1");
    };
    let shown = format!("{refused:#}");
    assert!(!shown.contains("api-key-marker"), "{shown}");
    assert!(shown.contains("<RPC_URL>"), "{shown}");
}
