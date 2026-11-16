//! The schema on databases of their own: built once on an empty one, and
//! refused where another file built it.
//!
//! Runs against the Postgres at `DATABASE_URL`, and fails without it; each
//! test creates and drops its own database there.

use std::str::FromStr;

use alloy::primitives::B256;
use sqlx::{
    postgres::PgConnectOptions,
    Executor,
    PgPool,
};
use usernames_core::db::{
    ConnectError,
    Schema,
    SCHEMA,
};
use usernames_fixtures::database_url;

/// A database of one test's own, created empty.
struct Empty {
    server: PgPool,
    name: &'static str,
    options: PgConnectOptions,
}

impl Empty {
    /// The database `name`, emptied of what an earlier run left.
    async fn open(name: &'static str) -> Self {
        let url = database_url();
        let server = PgPool::connect(&url).await.expect("connecting");
        server
            .execute(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)").as_str())
            .await
            .expect("dropping the earlier run's database");
        server
            .execute(format!("CREATE DATABASE {name}").as_str())
            .await
            .expect("creating the database");
        let options = PgConnectOptions::from_str(&url)
            .expect("DATABASE_URL")
            .database(name);
        Self {
            server,
            name,
            options,
        }
    }

    /// A new pool on the database.
    async fn pool(&self) -> PgPool {
        PgPool::connect_with(self.options.clone())
            .await
            .expect("connecting to the test's database")
    }

    /// Drop the database.
    async fn close(self) {
        let name = self.name;
        self.server
            .execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
            .await
            .expect("dropping the database");
    }
}

#[tokio::test]
async fn two_first_starts_on_an_empty_database_build_it_once() {
    let db = Empty::open("usernames_schema_first_starts").await;
    let (a, b) = (db.pool().await, db.pool().await);
    assert!(matches!(SCHEMA.check(&a).await, Err(ConnectError::Unbuilt)));

    let (built_a, built_b) = tokio::join!(SCHEMA.build(&a), SCHEMA.build(&b));
    built_a.expect("the first start");
    built_b.expect("the second start");

    let fingerprints: Vec<B256> =
        sqlx::query_scalar("SELECT fingerprint FROM names.schema")
            .fetch_all(&a)
            .await
            .expect("the fingerprints");
    assert_eq!(fingerprints, [SCHEMA.fingerprint()]);
    SCHEMA.check(&b).await.expect("the reader's check");
    db.close().await;
}

#[tokio::test]
async fn a_database_another_schema_built_is_refused() {
    let db = Empty::open("usernames_schema_another").await;
    let pool = db.pool().await;
    let other = Schema::of(
        "CREATE SCHEMA names; CREATE TABLE names.schema (fingerprint BYTEA NOT NULL);",
    );
    other.build(&pool).await.expect("the other schema");

    let refused = |found: Result<(), ConnectError>| {
        matches!(
            found,
            Err(ConnectError::Schema { found: Some(found), built })
                if found == other.fingerprint() && built == SCHEMA.fingerprint()
        )
    };
    assert!(refused(SCHEMA.build(&pool).await));
    assert!(refused(SCHEMA.check(&pool).await));
    db.close().await;
}

#[tokio::test]
async fn a_schema_without_a_fingerprint_is_refused() {
    let db = Empty::open("usernames_schema_unfingerprinted").await;
    let pool = db.pool().await;
    pool.execute("CREATE SCHEMA names")
        .await
        .expect("a schema without a fingerprint");

    let refused = |found: Result<(), ConnectError>| {
        matches!(
            found,
            Err(ConnectError::Schema { found: None, built }) if built == SCHEMA.fingerprint()
        )
    };
    assert!(refused(SCHEMA.build(&pool).await));
    assert!(refused(SCHEMA.check(&pool).await));
    db.close().await;
}
