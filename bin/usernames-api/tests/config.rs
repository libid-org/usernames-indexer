//! A configuration the server refuses before it serves.

use clap::Parser;

use sqlx::postgres::PgPoolOptions;
use usernames_api::*;

/// A pool that never dials. Every case below is refused before any query
/// runs, which is the point: these are startup checks, so they must not
/// need a database to reject a configuration that cannot work. It still
/// wants a runtime to be built in, which is why the cases are async.
fn lazy_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .expect("lazy pool")
}

/// A signer key is what turns the route on, so every case supplies one.
fn config(extra: &[&str]) -> Config {
    let mut argv = vec![
        "usernames-api",
        "--database-url",
        "postgres://u:p@127.0.0.1:5432/db",
        "--ens-signer-key",
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        "--ens-resolver-address",
        "0x0000000000000000000000000000000000000001",
    ];
    argv.extend_from_slice(extra);
    Config::try_parse_from(argv).expect("parse")
}

/// `ens::Config` holds a signer and so is not `Debug`; match rather than
/// `expect_err`.
async fn refusal(extra: &[&str]) -> String {
    match config(extra).gateway(lazy_pool()).await {
        Ok(_) => panic!("this configuration must be refused"),
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn without_a_signer_key_there_is_no_gateway() {
    let config = Config::try_parse_from([
        "usernames-api",
        "--database-url",
        "postgres://u:p@127.0.0.1:5432/db",
    ])
    .expect("parse");
    let mounted = config
        .gateway(lazy_pool())
        .await
        .expect("no key is not an error")
        .is_some();
    assert!(!mounted, "the route must not be mounted without a key");
}

/// A hex value of the wrong length is a mangled key, not a KMS id; it
/// must be refused here, before anything is handed to AWS.
#[tokio::test]
async fn a_mangled_hex_key_is_refused_before_kms_is_asked() {
    let config = Config::try_parse_from([
        "usernames-api",
        "--database-url",
        "postgres://u:p@127.0.0.1:5432/db",
        "--ens-signer-key",
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff",
        "--ens-resolver-address",
        "0x0000000000000000000000000000000000000001",
    ])
    .expect("parse");
    let message = match config.gateway(lazy_pool()).await {
        Ok(_) => panic!("a 63-digit key must be refused"),
        Err(e) => e.to_string(),
    };
    assert!(message.contains("ENS_SIGNER_KEY"), "{message}");
    assert!(message.contains("64"), "{message}");
}

#[tokio::test]
async fn a_ttl_the_resolver_would_reject_is_refused() {
    let message = refusal(&["--ens-ttl", "2h"]).await;
    assert!(message.contains("MAX_LIFETIME"), "{message}");
}

/// The ceiling leaves room for the chain to trail. A TTL equal to the
/// resolver's `MAX_LIFETIME` is refused, because it dates every answer past
/// what the contract accepts at any block older than this instant — which
/// is every block.
#[tokio::test]
async fn the_ttl_ceiling_leaves_the_chain_room_to_trail() {
    let at_ceiling = config(&[
        "--ens-ttl",
        &humantime::format_duration(MAX_TTL).to_string(),
    ]);
    assert!(at_ceiling.gateway(lazy_pool()).await.is_ok());

    let ceiling = humantime::format_duration(RESOLVER_MAX_LIFETIME).to_string();
    let message = refusal(&["--ens-ttl", &ceiling]).await;
    assert!(message.contains("trail"), "{message}");
}
