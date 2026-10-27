//! A suite's chain in the database the suites share, events applied through
//! the indexer's own window, and one request driven through the real router,
//! answered in the type the API declares for it.

use alloy::primitives::{
    Address,
    B256,
};
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
};
use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use sqlx::{
    postgres::PgListener,
    PgPool,
};
use std::{
    fmt::Debug,
    future::Future,
    time::Duration,
};
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
            ErrorCode,
            HistoryEntry,
            HistoryEvent,
        },
    },
    db::{
        self,
        ChainState,
        ChainStore,
        ChainWriter,
        Store,
    },
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes::{
        DisclosedHandle,
        KnownPlatform,
    },
};

use crate::{
    IdNode,
    Seed,
};

/// `DATABASE_URL`, or `None` to skip the test; under CI, where the job
/// provides Postgres, its absence fails the test instead.
pub fn database_url() -> Option<String> {
    let url = std::env::var("DATABASE_URL").ok();
    assert!(
        url.is_some() || std::env::var_os("CI").is_none(),
        "DATABASE_URL not set: CI runs this suite against Postgres"
    );
    url
}

/// Whether `anvil` runs; under CI, where the job installs it, a missing
/// anvil fails the test instead.
pub fn anvil_available() -> bool {
    let found = std::process::Command::new("anvil")
        .arg("--version")
        .output()
        .is_ok();
    assert!(
        found || std::env::var_os("CI").is_none(),
        "anvil not on PATH: CI runs this suite against anvil"
    );
    found
}

/// One suite's chain in the database the suites share: emptied of whatever
/// an earlier run left there, and held under the suite's lock until
/// [`Suite::done`].
pub struct Suite {
    /// The right to write the suite's chain, from [`Seed::writer`].
    pub writer: ChainWriter,
    /// The database the suites share.
    pub pool: PgPool,
    lock: MutexGuard<'static, ()>,
}

impl Suite {
    /// The suite's `chain`, emptied; `None` when `DATABASE_URL` is unset.
    pub async fn open(lock: &'static Mutex<()>, chain: i64) -> Option<Self> {
        let lock = lock.lock().await;
        let url = database_url()?;
        let pool = PgPool::connect(&url)
            .await
            .expect("DATABASE_URL is set but connecting failed");
        db::MIGRATOR.run(&pool).await.expect("migrations failed");
        let store = ChainStore::new(pool.clone(), chain);
        store.forget().await;
        Some(Self {
            writer: store.writer().await,
            pool,
            lock,
        })
    }

    /// The suite's chain.
    pub fn store(&self) -> &ChainStore {
        self.writer.store()
    }

    /// [`Writes::apply_tx`] on the suite's chain.
    pub async fn apply_tx(&self, block: u64, events: Vec<NamesEvent>) {
        self.writer.apply_tx(block, events).await;
    }

    /// [`Writes::apply`] on the suite's chain.
    pub async fn apply(&self, block: u64, event: NamesEvent) {
        self.writer.apply(block, event).await;
    }

    /// One GET on the suite's chain.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Reply<T> {
        self.pool.get_on(self.store().chain_id(), path).await
    }

    /// The test is done with the database; the suite's next test may take it.
    pub fn done(self) {
        let Self { lock, .. } = self;
        drop(lock);
    }
}

/// A chain written through its window, the way the indexer writes it.
pub trait Writes {
    /// Apply one transaction's events at `block`, in log order, and commit
    /// like a window. A block's time is its number past a constant, so a
    /// later block is later in time too.
    fn apply_tx(
        &self,
        block: u64,
        events: Vec<NamesEvent>,
    ) -> impl Future<Output = ()> + Send;

    /// Apply one event alone in its block.
    fn apply(&self, block: u64, event: NamesEvent) -> impl Future<Output = ()> + Send;
}

impl Writes for ChainWriter {
    async fn apply_tx(&self, block: u64, events: Vec<NamesEvent>) {
        let mut window = self.begin_window().await.expect("begin");
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

    async fn apply(&self, block: u64, event: NamesEvent) {
        self.apply_tx(block, vec![event]).await;
    }
}

/// What a suite reads through the database: the API's answers, and rows
/// the API does not serve.
pub trait Reads {
    /// One GET on the real router.
    fn get<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> impl Future<Output = Reply<T>> + Send;

    /// One GET scoped to `chain`: `?chain=` appended (`&` when the path
    /// already has a query).
    fn get_on<T: DeserializeOwned>(
        &self,
        chain: i64,
        path: &str,
    ) -> impl Future<Output = Reply<T>> + Send;

    /// The text the chain disclosed for the node on `chain`, when it did.
    fn disclosed(
        &self,
        chain: i64,
        handle_node: B256,
    ) -> impl Future<Output = Option<String>> + Send;
}

impl Reads for PgPool {
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Reply<T> {
        let response = api::AppState::new(Store::new(self.clone()))
            .router()
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

    async fn get_on<T: DeserializeOwned>(&self, chain: i64, path: &str) -> Reply<T> {
        let separator = if path.contains('?') { '&' } else { '?' };
        self.get(&format!("{path}{separator}chain={chain}")).await
    }

    async fn disclosed(&self, chain: i64, handle_node: B256) -> Option<String> {
        sqlx::query_scalar(include_str!("disclosed.sql"))
            .bind(chain)
            .bind(handle_node)
            .fetch_optional(self)
            .await
            .expect("a disclosure read")
    }
}

/// The events the contracts emit, as the suites write them.
pub trait Events: Sized {
    /// A bind's events, as the contract emits them: `IdentityBound`, and
    /// when the ceremony disclosed the handle as the holder's name,
    /// `HandleRevealed` and `HandlePublished`.
    fn bind(
        holder: Address,
        platform: KnownPlatform,
        id: &str,
        handle: &str,
        observed_at: u64,
        disclosed: bool,
    ) -> Vec<Self>;

    /// `publish(handle)`'s events: the text, then the name.
    fn publish(holder: Address, platform: KnownPlatform, handle: &str) -> Vec<Self>;

    /// The `HandleRevealed` of the handle as typed: from `reveal`, `publish`
    /// or a disclosing bind, or from a `depositToHandle`.
    fn revealed(platform: KnownPlatform, typed: &str) -> Self;

    /// A bind's retirement of the handle `holder` held.
    fn retire(platform: KnownPlatform, handle: &str, holder: Address) -> Self;

    /// A withdrawal of `holder`'s name.
    fn unpublish(holder: Address) -> Self;
}

impl Events for NamesEvent {
    fn bind(
        holder: Address,
        platform: KnownPlatform,
        id: &str,
        handle: &str,
        observed_at: u64,
        disclosed: bool,
    ) -> Vec<Self> {
        let mut events = vec![Self::IdentityBound {
            holder,
            id_node: platform.id_node(id),
            handle_node: platform.handle_node(handle).expect("a handle"),
            platform_id: platform.id(),
            observed_at,
            ceremony_version: 1,
        }];
        if disclosed {
            events.extend(Self::publish(holder, platform, handle));
        }
        events
    }

    fn publish(holder: Address, platform: KnownPlatform, handle: &str) -> Vec<Self> {
        let handle_node = platform.handle_node(handle).expect("a handle");
        vec![
            Self::revealed(platform, handle),
            Self::HandlePublished {
                holder,
                platform_id: platform.id(),
                handle_node,
            },
        ]
    }

    fn revealed(platform: KnownPlatform, typed: &str) -> Self {
        let (_, handle_node) = platform.folded_handle_node(typed);
        Self::HandleRevealed {
            platform_id: platform.id(),
            handle_node,
            handle: DisclosedHandle::verify(platform, handle_node, typed),
        }
    }

    fn retire(platform: KnownPlatform, handle: &str, holder: Address) -> Self {
        Self::HandleRetired {
            platform_id: platform.id(),
            handle_node: platform.handle_node(handle).expect("a handle"),
            holder,
        }
    }

    fn unpublish(holder: Address) -> Self {
        Self::HandleUnpublished { holder }
    }
}

/// How long a suite waits for the indexer to do something.
pub const PATIENCE: Duration = Duration::from_secs(30);

/// One chain's row, followed through the notices of its changes.
pub struct ChainWatch {
    listener: PgListener,
    store: ChainStore,
}

impl ChainWatch {
    /// Follow `store`'s chain from now on.
    pub async fn new(pool: &PgPool, store: &ChainStore) -> Self {
        let mut listener = PgListener::connect_with(pool).await.expect("a listener");
        listener
            .listen(db::CHAIN_CHANNEL)
            .await
            .expect("the chain channel");
        Self {
            listener,
            store: store.clone(),
        }
    }

    /// The chain's row after its next change. A listener that lost its
    /// session reconnects and may have missed notices in between, so the
    /// row is read again then too.
    pub async fn next(&mut self) -> ChainState {
        let chain = self.store.chain_id().to_string();
        loop {
            match self.listener.try_recv().await.expect("a notice") {
                Some(notice) if notice.payload() != chain => continue,
                Some(_) | None => {
                    return self.store.state().await.expect("the chain's row");
                }
            }
        }
    }

    /// The chain's row once `reached` holds of it: read now, then after
    /// each change, for at most [`PATIENCE`].
    pub async fn until(&mut self, reached: impl Fn(&ChainState) -> bool) -> ChainState {
        tokio::time::timeout(PATIENCE, async {
            let mut state = self.store.state().await.expect("the chain's row");
            while !reached(&state) {
                state = self.next().await;
            }
            state
        })
        .await
        .expect("the chain's row got there in time")
    }

    /// The chain's row once a window committed through `block`.
    pub async fn cursor_at(&mut self, block: u64) -> ChainState {
        self.until(|state| {
            state
                .position()
                .cursor
                .is_some_and(|cursor| cursor >= block)
        })
        .await
    }
}

/// A history page's entries.
pub trait Entries {
    /// The entries' events, in the order served.
    fn events(&self) -> Vec<&HistoryEvent>;
}

impl Entries for [HistoryEntry] {
    fn events(&self) -> Vec<&HistoryEvent> {
        self.iter().map(|entry| &entry.event).collect()
    }
}

/// What one GET came back with: the answer in its type, or the refusal.
pub struct Reply<T> {
    /// The status the router answered with.
    pub status: StatusCode,
    /// The answer in its type, or the refusal in the API's error envelope.
    pub body: Result<T, ErrorBody>,
}

impl<T: Debug> Reply<T> {
    /// The answer; a refusal here fails the test with its code and message.
    pub fn answer(self) -> T {
        match self.body {
            Ok(answer) => answer,
            Err(refusal) => panic!(
                "{}: {:?}: {}",
                self.status, refusal.error.code, refusal.error.message
            ),
        }
    }

    /// The status and code of a refusal; an answer here fails the test.
    pub fn refusal(self) -> (StatusCode, ErrorCode) {
        match self.body {
            Err(refusal) => (self.status, refusal.error.code),
            Ok(answer) => panic!("answered {}: {answer:?}", self.status),
        }
    }
}
