//! The schema on databases of their own: migration `001` builds an empty one,
//! and a database that applied other migrations, or another `001`, is
//! refused untouched.
//!
//! Runs against the Postgres at `DATABASE_URL`, and fails without it; each
//! test creates and drops its own database there.

use std::{
    borrow::Cow,
    str::FromStr,
};

use sqlx::{
    migrate::{
        MigrateError,
        Migration,
        MigrationType,
        Migrator,
    },
    postgres::PgConnectOptions,
    Executor,
    PgPool,
};
use usernames_core::db::MIGRATOR;
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

/// Another release's migrations: `versions`, with a `001` that differs from
/// this build's.
struct Release(Migrator);

impl Release {
    fn of(versions: &[i64]) -> Self {
        let migrations = versions
            .iter()
            .map(|&version| {
                let sql = match version {
                    1 => "CREATE SCHEMA names",
                    _ => "SELECT 1",
                };
                Migration::new(
                    version,
                    Cow::Borrowed("another release"),
                    MigrationType::Simple,
                    Cow::Borrowed(sql),
                    false,
                )
            })
            .collect::<Vec<_>>();
        Self(Migrator {
            migrations: Cow::Owned(migrations),
            ..Migrator::DEFAULT
        })
    }

    /// The database as this release migrated it.
    async fn migrate(&self, pool: &PgPool) {
        self.0
            .run(pool)
            .await
            .expect("the other release's migrations");
    }
}

/// What a test reads of a database's schema.
trait Schema {
    /// The migration versions the database applied.
    async fn applied(&self) -> Vec<i64>;
    /// Whether this build's tables exist.
    async fn built(&self) -> bool;
}

impl Schema for PgPool {
    async fn applied(&self) -> Vec<i64> {
        sqlx::query_scalar(include_str!("sql/applied_migrations.sql"))
            .fetch_all(self)
            .await
            .expect("the applied migrations")
    }

    async fn built(&self) -> bool {
        sqlx::query_scalar(include_str!("sql/chains_exists.sql"))
            .fetch_one(self)
            .await
            .expect("whether names.chains exists")
    }
}

#[tokio::test]
async fn two_first_starts_on_an_empty_database_build_it_once() {
    let db = Empty::open("usernames_migrations_first_starts").await;
    let (a, b) = (db.pool().await, db.pool().await);

    let (built_a, built_b) = tokio::join!(MIGRATOR.run(&a), MIGRATOR.run(&b));
    built_a.expect("the first start");
    built_b.expect("the second start");
    MIGRATOR.run(&a).await.expect("a restart");

    assert_eq!(a.applied().await, [1]);
    assert!(a.built().await);
    db.close().await;
}

#[tokio::test]
async fn a_database_with_migrations_this_build_lacks_is_refused() {
    let db = Empty::open("usernames_migrations_earlier").await;
    let pool = db.pool().await;
    Release::of(&[1, 3, 4, 5, 6, 7]).migrate(&pool).await;

    assert!(matches!(
        MIGRATOR.run(&pool).await,
        Err(MigrateError::VersionMissing(3))
    ));
    assert_eq!(pool.applied().await, [1, 3, 4, 5, 6, 7]);
    assert!(!pool.built().await);
    db.close().await;
}

#[tokio::test]
async fn a_database_another_001_built_is_refused() {
    let db = Empty::open("usernames_migrations_another_001").await;
    let pool = db.pool().await;
    Release::of(&[1]).migrate(&pool).await;

    assert!(matches!(
        MIGRATOR.run(&pool).await,
        Err(MigrateError::VersionMismatch(1))
    ));
    assert!(!pool.built().await);
    db.close().await;
}
