//! Plumbing the integration suites share: a suite's chain in the database
//! they share, events applied through the indexer's own window, and one
//! request driven through the real router, answered in the type the API
//! declares for it.

// Every suite compiles its own copy of this module and uses part of it.
#![allow(dead_code)]

use alloy::primitives::B256;
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
};
use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tokio::sync::{
    Mutex,
    MutexGuard,
};
use tower::ServiceExt;
use usernames_core::{
    api::{
        self,
        model::{
            ErrorBody,
            HistoryEntry,
            HistoryEvent,
        },
    },
    db::{
        self,
        ChainStore,
        Store,
    },
    events::{
        LogPosition,
        NamesEvent,
    },
};

/// One suite's chain in the database the suites share: emptied of whatever
/// an earlier run left there, and held under the suite's lock until
/// [`Suite::done`].
pub struct Suite {
    pub store: ChainStore,
    pub pool: PgPool,
    lock: MutexGuard<'static, ()>,
}

impl Suite {
    /// The suite's `chain`, emptied; `None` when `DATABASE_URL` is unset.
    pub async fn open(lock: &'static Mutex<()>, chain: i64) -> Option<Self> {
        let lock = lock.lock().await;
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = PgPool::connect(&url)
            .await
            .expect("DATABASE_URL is set but connecting failed");
        db::MIGRATOR.run(&pool).await.expect("migrations failed");
        clear_chain(&pool, chain).await;
        Some(Self {
            store: ChainStore::new(pool.clone(), chain),
            pool,
            lock,
        })
    }

    /// Apply one transaction's events at `block`, in log order, and commit
    /// like a window. A block's time is its number past a constant, so a
    /// later block is later in time too.
    pub async fn apply_tx(&self, block: u64, events: Vec<NamesEvent>) {
        let mut window = self.store.begin_window().await.expect("begin");
        for (log_index, event) in (0u64..).zip(events) {
            let pos = LogPosition {
                block_number: block,
                log_index,
                tx_hash: B256::left_padding_from(&block.to_be_bytes()),
                block_time: 1_700_000_000 + block,
            };
            window.apply(&event, &pos).await.expect("apply");
        }
        window.commit(block).await.expect("commit");
    }

    /// Apply one event alone in its block.
    pub async fn apply(&self, block: u64, event: NamesEvent) {
        self.apply_tx(block, vec![event]).await;
    }

    /// One GET on the suite's chain.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Reply<T> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let chain = self.store.chain_id();
        get(&self.pool, &format!("{path}{separator}chain={chain}")).await
    }

    /// The test is done with the database; the suite's next test may take it.
    pub fn done(self) {
        let Self { lock, .. } = self;
        drop(lock);
    }
}

/// Delete every row the store holds for `chain`: what a replay clears, the
/// deployment-block cache a replay keeps, and the chain's names.
pub async fn clear_chain(pool: &PgPool, chain: i64) {
    let forget_metadata = include_str!("forget_chain_metadata.sql");
    for statement in [db::CLEAR_CHAIN, forget_metadata, db::CLEAR_CHAIN_NAMES] {
        sqlx::query(statement)
            .bind(chain)
            .execute(pool)
            .await
            .expect("cleanup");
    }
}

/// A history's events, in the order served.
pub fn events(entries: &[HistoryEntry]) -> Vec<&HistoryEvent> {
    entries.iter().map(|entry| &entry.event).collect()
}

/// What one GET came back with: the answer in its type, or the refusal.
pub struct Reply<T> {
    pub status: StatusCode,
    pub body: Result<T, ErrorBody>,
}

impl<T: std::fmt::Debug> Reply<T> {
    /// The answer; a refusal here fails the test with its code and message.
    pub fn answer(self) -> T {
        match self.body {
            Ok(answer) => answer,
            Err(refusal) => panic!(
                "{}: {}: {}",
                self.status, refusal.error.code, refusal.error.message
            ),
        }
    }

    /// The status and code of a refusal; an answer here fails the test.
    pub fn refusal(self) -> (StatusCode, String) {
        match self.body {
            Err(refusal) => (self.status, refusal.error.code),
            Ok(answer) => panic!("answered {}: {answer:?}", self.status),
        }
    }
}

/// One GET against the same axum router a caller would hit, decoded into the
/// type the API answers with — or, off the success range, into the error
/// envelope. A body in neither shape is a failed test, not a value.
pub async fn get<T: DeserializeOwned>(pool: &PgPool, path: &str) -> Reply<T> {
    let state = api::AppState::new(Store::new(pool.clone()));
    let response = api::router(state)
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = || String::from_utf8_lossy(&bytes).into_owned();
    let body = if status.is_success() {
        Ok(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!("{status}: not the answer's shape ({e}): {}", text())
        }))
    } else {
        Err(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!("{status}: not the error shape ({e}): {}", text())
        }))
    };
    Reply { status, body }
}
