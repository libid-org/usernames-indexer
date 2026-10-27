//! Plumbing the integration suites share: a suite's chain in the database
//! they share, events applied through the indexer's own window, and one
//! request driven through the real router, answered in the type the API
//! declares for it.

// Every suite compiles its own copy of this module and uses part of it.
#![allow(dead_code)]

pub mod libid;

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
    /// The right to write the suite's chain, from [`writer`].
    pub writer: ChainWriter,
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
        clear_chain(&pool, chain).await;
        Some(Self {
            writer: writer(&ChainStore::new(pool.clone(), chain)).await,
            pool,
            lock,
        })
    }

    /// The suite's chain.
    pub fn store(&self) -> &ChainStore {
        self.writer.store()
    }

    /// [`apply_tx`] on the suite's chain.
    pub async fn apply_tx(&self, block: u64, events: Vec<NamesEvent>) {
        apply_tx(&self.writer, block, events).await;
    }

    /// [`apply`] on the suite's chain.
    pub async fn apply(&self, block: u64, event: NamesEvent) {
        apply(&self.writer, block, event).await;
    }

    /// One GET on the suite's chain.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Reply<T> {
        get_on(&self.pool, self.store().chain_id(), path).await
    }

    /// The test is done with the database; the suite's next test may take it.
    pub fn done(self) {
        let Self { lock, .. } = self;
        drop(lock);
    }
}

/// Delete every row the store holds for `chain`.
pub async fn clear_chain(pool: &PgPool, chain: i64) {
    ChainStore::new(pool.clone(), chain).forget().await;
}

/// The node `id` is bound under on `platform`, as libid-identity derives it.
pub fn id_node(platform: KnownPlatform, id: &str) -> B256 {
    let spec =
        libid_identity::platforms::platform(platform.key()).expect("a libid platform");
    B256::from(spec.id_node(id).expect("an id"))
}

/// The registry a chain these suites write is prepared for.
pub const REGISTRY: Address = Address::repeat_byte(0x1D);

/// The right to write `store`'s chain, taken the way an indexer takes it at
/// startup: under the chain's lease, prepared for [`REGISTRY`]. The lease
/// goes back at once; the writer stays good until the chain is prepared
/// again.
pub async fn writer(store: &ChainStore) -> ChainWriter {
    let lease = store.acquire_writer().await.expect("the writer lease");
    let writer = store
        .prepare(&lease, REGISTRY, None)
        .await
        .expect("the chain is prepared");
    lease.release().await.expect("the lease goes back");
    writer
}

/// Apply one transaction's events at `block`, in log order, and commit
/// like a window. A block's time is its number past a constant, so a later
/// block is later in time too.
pub async fn apply_tx(writer: &ChainWriter, block: u64, events: Vec<NamesEvent>) {
    let mut window = writer.begin_window().await.expect("begin");
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
pub async fn apply(writer: &ChainWriter, block: u64, event: NamesEvent) {
    apply_tx(writer, block, vec![event]).await;
}

/// The text the chain disclosed for the node on `chain`, when it did.
pub async fn disclosed(pool: &PgPool, chain: i64, handle_node: B256) -> Option<String> {
    sqlx::query_scalar(
        "SELECT handle FROM names.handle_nodes WHERE chain_id = $1 AND handle_node = $2 AND handle IS NOT NULL",
    )
    .bind(chain)
    .bind(handle_node)
    .fetch_optional(pool)
    .await
    .expect("a disclosure read")
}

/// A bind's events, as the contract emits them: `IdentityBound`, and when
/// the ceremony disclosed the handle as the holder's name, `HandleRevealed`
/// and `HandlePublished`.
pub fn bind(
    holder: Address,
    platform: KnownPlatform,
    id: &str,
    handle: &str,
    observed_at: u64,
    disclosed: bool,
) -> Vec<NamesEvent> {
    let mut events = vec![NamesEvent::IdentityBound {
        holder,
        id_node: id_node(platform, id),
        handle_node: platform.handle_node(handle).expect("a handle"),
        platform_id: platform.id(),
        observed_at,
        ceremony_version: 1,
    }];
    if disclosed {
        events.extend(publish(holder, platform, handle));
    }
    events
}

/// `publish(handle)`'s events: the text, then the name.
pub fn publish(
    holder: Address,
    platform: KnownPlatform,
    handle: &str,
) -> Vec<NamesEvent> {
    let handle_node = platform.handle_node(handle).expect("a handle");
    vec![
        revealed(platform, handle),
        NamesEvent::HandlePublished {
            holder,
            platform_id: platform.id(),
            handle_node,
        },
    ]
}

/// The `HandleRevealed` of the handle as typed: from `reveal`, `publish` or a
/// disclosing bind, or from a `depositToHandle`.
pub fn revealed(platform: KnownPlatform, typed: &str) -> NamesEvent {
    let (_, handle_node) = platform.folded_handle_node(typed);
    NamesEvent::HandleRevealed {
        platform_id: platform.id(),
        handle_node,
        handle: DisclosedHandle::verify(platform, handle_node, typed),
    }
}

/// A bind's retirement of the handle `holder` held.
pub fn retire(platform: KnownPlatform, handle: &str, holder: Address) -> NamesEvent {
    NamesEvent::HandleRetired {
        platform_id: platform.id(),
        handle_node: platform.handle_node(handle).expect("a handle"),
        holder,
    }
}

/// A withdrawal of `holder`'s name.
pub fn unpublish(holder: Address) -> NamesEvent {
    NamesEvent::HandleUnpublished { holder }
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

    /// The chain's row after its next change.
    pub async fn next(&mut self) -> ChainState {
        let chain = self.store.chain_id().to_string();
        loop {
            let notice = self.listener.recv().await.expect("a notice");
            if notice.payload() == chain {
                return self.store.state().await.expect("the chain's row");
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

/// A history's events, in the order served.
pub fn events(entries: &[HistoryEntry]) -> Vec<&HistoryEvent> {
    entries.iter().map(|entry| &entry.event).collect()
}

/// What one GET came back with: the answer in its type, or the refusal.
pub struct Reply<T> {
    pub status: StatusCode,
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

/// One GET scoped to `chain`.
pub async fn get_on<T: DeserializeOwned>(
    pool: &PgPool,
    chain: i64,
    path: &str,
) -> Reply<T> {
    let separator = if path.contains('?') { '&' } else { '?' };
    get(pool, &format!("{path}{separator}chain={chain}")).await
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
