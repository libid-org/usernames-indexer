//! The read API. Resolution answers exactly what the contract's resolvers
//! answer, from the projections; search is the one thing the chain cannot do.

use std::str::FromStr;

use alloy::primitives::{
    Address,
    B256,
};
use axum::{
    extract::{
        Path,
        Query,
        State,
    },
    http::StatusCode,
    response::{
        IntoResponse,
        Response,
    },
    routing::get,
    Json,
    Router,
};
use serde::{
    Deserialize,
    Serialize,
};
use tower_http::cors::{
    Any,
    CorsLayer,
};

use crate::{
    db::{
        self,
        ChainStore,
    },
    nodes,
};

/// What every handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The chain-scoped store every query goes through.
    store: ChainStore,
    /// The watched contract, echoed in `/v1/status`.
    contract: Address,
}

impl AppState {
    /// State for one deployment: the chain the store is scoped to is the
    /// chain this API serves.
    pub fn new(store: ChainStore, contract: Address) -> Self {
        Self { store, contract }
    }
}

/// The router, ready to serve.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route(
            "/v1/resolve/handle/{platform}/{handle}",
            get(resolve_handle),
        )
        .route("/v1/resolve/id/{platform}/{user_id}", get(resolve_id))
        .route("/v1/resolve/address/{address}", get(resolve_address))
        .route("/v1/search", get(search))
        // A read-only public resolver: any origin may GET. This is what lets
        // a browser UI (handle.link) call the API cross-origin at all.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([axum::http::Method::GET]),
        )
        .with_state(state)
}

struct ApiError(StatusCode, String);

impl ApiError {
    fn not_found(msg: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, msg.into())
    }

    fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
}

impl ApiError {
    fn not_synced() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "the indexer has not finished a first window on this chain yet".into(),
        )
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(%e, "query failed");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn parse_platform(raw: &str) -> Result<nodes::Platform, ApiError> {
    nodes::Platform::parse(raw).map_err(|e| ApiError::bad_request(e.to_string()))
}

/// Postgres cannot compare TEXT holding a NUL byte, and no stored value ever
/// holds one, so refuse it at the edge instead of turning it into a 500.
fn reject_nul(raw: &str, what: &str) -> Result<(), ApiError> {
    if raw.contains('\0') {
        return Err(ApiError::bad_request(format!(
            "{what} must not contain a NUL byte"
        )));
    }
    Ok(())
}

/// Resolution answers come from a mirror, and a mirror that has never
/// committed a window would serve authoritative-looking 404s for names that
/// are bound on chain. Refuse to answer until the first window landed.
async fn ensure_synced(state: &AppState) -> Result<(), ApiError> {
    match state.store.cursor().await? {
        Some(_) => Ok(()),
        None => Err(ApiError::not_synced()),
    }
}

fn parse_address(raw: &str) -> Result<Address, ApiError> {
    Address::from_str(raw)
        .map_err(|_| ApiError::bad_request(format!("{raw:?} is not an address")))
}

fn address_from_db(bytes: &[u8]) -> String {
    Address::from_slice(bytes).to_string()
}

fn b256_from_db(bytes: &[u8]) -> String {
    B256::from_slice(bytes).to_string()
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    chain_id: i64,
    contract: String,
    last_indexed_block: Option<u64>,
    chain_head_block: Option<u64>,
    /// Blocks between the head the loop last saw and the cursor. Small and
    /// steady is healthy; growing means the loop is stalled or starved —
    /// `last_window_error` says which.
    lag_blocks: Option<u64>,
    last_window_error: Option<String>,
    indexer_version: &'static str,
}

async fn status(State(state): State<AppState>) -> Result<Json<Status>, ApiError> {
    let last = state.store.cursor().await?;
    let head = state.store.chain_head().await?;
    Ok(Json(Status {
        chain_id: state.store.chain_id(),
        contract: state.contract.to_string(),
        last_indexed_block: last,
        chain_head_block: head,
        lag_blocks: head.map(|h| h.saturating_sub(last.unwrap_or(0))),
        last_window_error: state.store.window_error().await?,
        indexer_version: db::INDEXER_VERSION,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HandleResolution {
    platform: Option<&'static str>,
    platform_id: String,
    handle: String,
    handle_node: String,
    owner: String,
    observed_at: i64,
    version: i64,
    user_id: Option<String>,
    id_node: String,
    /// Mirrors `resolvePair`: the account id this handle points back at still
    /// resolves to the same wallet.
    id_agrees: bool,
}

/// `GET /v1/resolve/handle/{platform}/{handle}` — the wallet a handle
/// resolves to, the way `resolveHandle` would answer it.
async fn resolve_handle(
    State(state): State<AppState>,
    Path((platform, handle)): Path<(String, String)>,
) -> Result<Json<HandleResolution>, ApiError> {
    let platform = parse_platform(&platform)?;
    reject_nul(&handle, "handle")?;
    ensure_synced(&state).await?;

    // Normalize the way the chain did before it keyed the handle. Text the
    // platform could never hold mirrors the contract's `resolveHandle`,
    // which deliberately answers the zero address for it — a 404, not a 400:
    // "nobody holds this", not "you asked wrong".
    let normalized = match platform.normalize_query(&handle) {
        Ok(normalized) => normalized,
        Err(e) => {
            return Err(ApiError::not_found(format!(
                "no handle can exist on this platform for this text: {e}"
            )));
        }
    };

    let row = state
        .store
        .resolve_handle(platform.id(), &normalized)
        .await?;
    let Some(row) = row else {
        // Distinguish the contract's UnknownPlatform revert from its
        // zero-address answer: an unwired platform is a different fact than
        // an unclaimed handle.
        if !state.store.platform_wired(platform.id()).await? {
            return Err(ApiError::not_found(
                "this platform is not configured on this chain",
            ));
        }
        return Err(ApiError::not_found(format!(
            "{:?} is not bound",
            normalized.as_str()
        )));
    };
    let id_agrees = row.id_agrees();
    let Some(owner) = row.owner else {
        return Err(ApiError::not_found(format!(
            "{:?} was retired: its account proved a different handle",
            normalized.as_str()
        )));
    };

    Ok(Json(HandleResolution {
        platform: platform.key(),
        platform_id: platform.id().to_string(),
        handle: row.handle,
        handle_node: b256_from_db(&row.handle_node),
        owner: address_from_db(&owner),
        observed_at: row.observed_at,
        version: row.version,
        user_id: row.user_id,
        id_node: b256_from_db(&row.id_node),
        id_agrees,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IdResolution {
    platform: Option<&'static str>,
    platform_id: String,
    user_id: String,
    id_node: String,
    owner: String,
    observed_at: i64,
    version: i64,
    /// The handle this account last proved, when the node it points at is
    /// still the account's — the `handleOfId`/`idOfHandle` round trip.
    handle: Option<String>,
    handle_node: String,
    published: bool,
}

/// `GET /v1/resolve/id/{platform}/{user_id}` — the wallet an account id
/// resolves to, the way `resolveId` would answer it. The id is matched
/// byte-verbatim, exactly as the chain keys it.
async fn resolve_id(
    State(state): State<AppState>,
    Path((platform, user_id)): Path<(String, String)>,
) -> Result<Json<IdResolution>, ApiError> {
    let platform = parse_platform(&platform)?;
    reject_nul(&user_id, "userId")?;
    ensure_synced(&state).await?;

    let row = state.store.resolve_id(platform.id(), &user_id).await?;
    let Some(row) = row else {
        if !state.store.platform_wired(platform.id()).await? {
            return Err(ApiError::not_found(
                "this platform is not configured on this chain",
            ));
        }
        return Err(ApiError::not_found(format!("{user_id:?} is not bound")));
    };

    Ok(Json(IdResolution {
        platform: platform.key(),
        platform_id: platform.id().to_string(),
        id_node: b256_from_db(&row.id_node),
        owner: address_from_db(&row.owner),
        observed_at: row.observed_at,
        version: row.version,
        handle: row.presentable_handle().map(str::to_string),
        handle_node: b256_from_db(&row.handle_node),
        published: row.displayed(),
        user_id: row.user_id,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddressIdentity {
    platform: Option<&'static str>,
    platform_id: String,
    user_id: String,
    handle: Option<String>,
    handle_node: String,
    observed_at: i64,
    version: i64,
    /// Whether the handle still resolves to this wallet.
    resolves: bool,
    /// Whether this is the wallet's displayed name on the platform.
    published: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddressResolution {
    address: String,
    identities: Vec<AddressIdentity>,
}

/// `GET /v1/resolve/address/{address}` — every identity a wallet proved,
/// with the published flag that mirrors `primaryOf`'s reverse display.
async fn resolve_address(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> Result<Json<AddressResolution>, ApiError> {
    let address = parse_address(&address)?;
    ensure_synced(&state).await?;

    let rows = state.store.identities_of(address).await?;
    let mut identities = Vec::with_capacity(rows.len());
    for row in rows {
        let platform_id = B256::from_slice(&row.platform_id);
        identities.push(AddressIdentity {
            platform: nodes::Platform::key_of(platform_id),
            platform_id: platform_id.to_string(),
            handle: row.presentable_handle().map(str::to_string),
            handle_node: b256_from_db(&row.handle_node),
            observed_at: row.observed_at,
            version: row.version,
            resolves: row.handle_still_owned(),
            published: row.displayed(),
            user_id: row.user_id,
        });
    }

    Ok(Json(AddressResolution {
        address: address.to_string(),
        identities,
    }))
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    platform: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchHit {
    platform: Option<&'static str>,
    platform_id: String,
    handle: String,
    owner: String,
    user_id: Option<String>,
    published: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResults {
    query: String,
    hits: Vec<SearchHit>,
}

/// `GET /v1/search?q=gre&platform=x&limit=10` — matching variants for a
/// partial handle, exact first, then prefix, then substring and fuzzy.
async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResults>, ApiError> {
    reject_nul(&params.q, "q")?;
    ensure_synced(&state).await?;
    let query = nodes::fold_search_query(&params.q);
    if query.is_empty() {
        return Err(ApiError::bad_request("q must be nonempty"));
    }
    let limit = params.limit.unwrap_or(10).clamp(1, 50);
    let platform_id = params
        .platform
        .as_deref()
        .map(parse_platform)
        .transpose()?
        .map(|p| p.id());

    let rows = state
        .store
        .search_handles(platform_id, &query, limit)
        .await?;
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let platform_id = B256::from_slice(&row.platform_id);
        hits.push(SearchHit {
            platform: nodes::Platform::key_of(platform_id),
            platform_id: platform_id.to_string(),
            handle: row.handle,
            owner: address_from_db(&row.owner),
            user_id: row.user_id,
            published: row.published,
        });
    }

    Ok(Json(SearchResults { query, hits }))
}
