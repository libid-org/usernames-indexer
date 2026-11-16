//! The stats rollups' own reads.

use std::time::Duration;

use sqlx::PgPool;
use usernames_fixtures::database_url;

use usernames_core::db::stats::*;

/// A computation's statements share its deadline: the second gets only
/// what the first left, and the database stops it there.
#[tokio::test]
async fn the_statements_share_one_deadline() {
    let url = database_url();
    let pool = PgPool::connect(&url).await.expect("connect");
    let mut tx = pool.begin().await.expect("transaction");
    let deadline = Deadline::after(Duration::from_secs(1));
    sqlx::query("SELECT pg_sleep(0.6)")
        .execute(deadline.bound(&mut tx).await.expect("bounded"))
        .await
        .expect("the first statement ends in time");
    let stopped = sqlx::query("SELECT pg_sleep(0.6)")
        .execute(deadline.bound(&mut tx).await.expect("bounded"))
        .await
        .expect_err("the second statement outlasts the deadline");
    let code = stopped.as_database_error().and_then(|error| error.code());
    // query_canceled: the statement timeout stopped it on the server.
    assert_eq!(code.as_deref(), Some("57014"), "{stopped}");
}

/// A listed address reads from its `<chain id>:<address>` text and
/// prints back to it; anything else is refused.
#[test]
fn a_listed_address_parses_from_its_text() {
    let text = "8453:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    let listed: ListedAddress = text.parse().expect("a listed address");
    assert_eq!(listed.chain_id, 8453);
    assert_eq!(listed.to_string(), text);
    for refused in [
        "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        "base:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        "8453:0x8335",
    ] {
        assert!(refused.parse::<ListedAddress>().is_err(), "{refused}");
    }
}
