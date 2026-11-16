//! The read model's schema: one file, built once on an empty database and
//! named there by its fingerprint.

use std::sync::LazyLock;

use alloy::primitives::{
    keccak256,
    B256,
};
use sqlx::{
    PgConnection,
    PgPool,
};

use super::{
    sql,
    ConnectError,
};

/// The schema this build writes and reads.
pub static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::of(sql::SCHEMA));

/// A schema file and its fingerprint, the keccak256 of its text, which a
/// database it built keeps in `names.schema`.
#[derive(Debug)]
pub struct Schema {
    text: &'static str,
    fingerprint: B256,
}

/// What a database holds where the schema goes.
enum Holds {
    /// No `names` schema: the database is empty.
    Nothing,
    /// A `names` schema, with the fingerprint it keeps, if any.
    Schema(Option<B256>),
}

/// Whether a database holds the schema, and its fingerprint.
#[derive(sqlx::FromRow)]
struct Found {
    /// The `names` schema exists.
    built: bool,
    /// It keeps a fingerprint.
    fingerprinted: bool,
}

impl Schema {
    /// The schema `text` builds.
    pub fn of(text: &'static str) -> Self {
        Self {
            text,
            fingerprint: keccak256(text),
        }
    }

    /// The keccak256 of the schema's text.
    #[cfg(feature = "test-helpers")]
    pub fn fingerprint(&self) -> B256 {
        self.fingerprint
    }

    /// Build the schema on an empty database, or check the one there.
    /// Indexers starting on one empty database at once build it once: each
    /// waits on one lock, the first builds, the rest find its fingerprint.
    pub async fn build(&self, pool: &PgPool) -> Result<(), ConnectError> {
        let mut tx = pool.begin().await?;
        sqlx::query(sql::SCHEMA_LOCK).execute(&mut *tx).await?;
        match Holds::read(&mut tx).await? {
            Holds::Nothing => {
                sqlx::raw_sql(self.text).execute(&mut *tx).await?;
                sqlx::query(sql::SCHEMA_FINGERPRINT_SET)
                    .bind(self.fingerprint)
                    .execute(&mut *tx)
                    .await?;
            }
            Holds::Schema(found) => self.matches(found)?,
        }
        tx.commit().await?;
        Ok(())
    }

    /// Check the schema the database holds, without changing it: a reader
    /// needs no right to build one.
    pub async fn check(&self, pool: &PgPool) -> Result<(), ConnectError> {
        let mut conn = pool.acquire().await?;
        match Holds::read(&mut conn).await? {
            Holds::Nothing => Err(ConnectError::Unbuilt),
            Holds::Schema(found) => self.matches(found),
        }
    }

    /// Refuse a schema this file did not build.
    fn matches(&self, found: Option<B256>) -> Result<(), ConnectError> {
        if found == Some(self.fingerprint) {
            return Ok(());
        }
        Err(ConnectError::Schema {
            found,
            built: self.fingerprint,
        })
    }
}

impl Holds {
    /// What the database behind `conn` holds.
    async fn read(conn: &mut PgConnection) -> Result<Self, sqlx::Error> {
        let found: Found = sqlx::query_as(sql::SCHEMA_FOUND)
            .fetch_one(&mut *conn)
            .await?;
        if !found.built {
            return Ok(Self::Nothing);
        }
        if !found.fingerprinted {
            return Ok(Self::Schema(None));
        }
        let fingerprint = sqlx::query_scalar(sql::SCHEMA_FINGERPRINT)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(Self::Schema(fingerprint))
    }
}
