//! The read API. Resolution answers exactly what the contract's resolvers
//! answer, from the projections; search is the one thing the chain cannot do.
//! The answers' shapes live in [`model`]; the handlers here only translate
//! HTTP to store calls and rows to those shapes.

pub mod model;

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
use serde::Deserialize;

use self::model::*;
use crate::{
    db::{
        self,
        ChainStore,
        Store,
    },
    nodes,
};

/// What every handler needs: the store. Which chains it holds, and which
/// contract each was indexed from, is read from what the indexers wrote.
#[derive(Clone)]
pub struct AppState {
    store: Store,
}

impl AppState {
    /// State over one database, however many chains it holds.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// The chains a request reads from — the one `?chain=` names, or all —
    /// once an indexer has committed a first window there. Asked per
    /// request: the indexer may wipe and replay a chain at any moment, and a
    /// cached answer would then be served over an empty read model as
    /// authoritative-looking 404s.
    async fn synced(&self, chain: Option<i64>) -> Result<(), ApiError> {
        if self.store.synced(chain).await? {
            Ok(())
        } else {
            Err(ApiError::not_synced(chain))
        }
    }
}

/// The routes, without middleware.
///
/// No CORS layer here on purpose: a `layer` wraps only the routes already on
/// the router it is called on, so one applied inside this function cannot
/// cover anything a caller merges afterwards. The binary mounts every route
/// first and applies CORS once over the whole thing.
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
        .with_state(state)
}

/// One error shape for the whole API: a status, a stable code, and prose.
/// The code is the machine-readable half of the contract — a UI that renders
/// "retired" differently from "never claimed" branches on it, not on English
/// sentences that may be reworded.
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// The operator-facing cause, logged when the response renders and never
    /// serialized to the client.
    source: Option<sqlx::Error>,
}

impl ApiError {
    fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code,
            message: message.into(),
            source: None,
        }
    }

    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
            source: None,
        }
    }

    fn not_synced(chain: Option<i64>) -> Self {
        let message = match chain {
            Some(chain) => {
                format!("no indexer has finished a first window on chain {chain}")
            }
            None => "no indexer has finished a first window on any chain".to_string(),
        };
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "not_synced",
            message,
            source: None,
        }
    }
}

impl From<sqlx::Error> for ApiError {
    // A pure conversion: the logging happens where the error is handled
    // (IntoResponse), not as a side effect of the type change.
    fn from(e: sqlx::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "internal error".into(),
            source: Some(e),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Some(e) = &self.source {
            tracing::error!(%e, "query failed");
        }
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.to_string(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

fn parse_platform(raw: &str) -> Result<nodes::Platform, ApiError> {
    nodes::Platform::parse(raw)
        .map_err(|e| ApiError::bad_request("invalid_platform", e.to_string()))
}

/// Postgres cannot compare TEXT holding a NUL byte, and no stored value ever
/// holds one, so refuse it at the edge instead of turning it into a 500.
fn reject_nul(raw: &str, what: &str) -> Result<(), ApiError> {
    if raw.contains('\0') {
        return Err(ApiError::bad_request(
            "invalid_argument",
            format!("{what} must not contain a NUL byte"),
        ));
    }
    Ok(())
}

/// `?chain=8453` narrows a read to one chain; absent, every chain the store
/// holds answers, and each result says which chain it is from.
#[derive(Deserialize)]
struct ChainFilter {
    chain: Option<String>,
}

impl ChainFilter {
    fn parse(&self) -> Result<Option<i64>, ApiError> {
        self.chain
            .as_deref()
            .map(|raw| {
                raw.parse().map_err(|_| {
                    ApiError::bad_request(
                        "invalid_chain",
                        format!("{raw:?} is not a chain id"),
                    )
                })
            })
            .transpose()
    }
}

fn parse_address(raw: &str) -> Result<Address, ApiError> {
    Address::from_str(raw).map_err(|_| {
        ApiError::bad_request("invalid_address", format!("{raw:?} is not an address"))
    })
}

async fn health() -> &'static str {
    "ok"
}

impl ChainStatus {
    async fn of(store: &ChainStore) -> Result<Self, ApiError> {
        let last = store.cursor().await?;
        let head = store.chain_head().await?;
        let position = store.index_position().await?;
        Ok(Self {
            chain_id: store.chain_id(),
            contract: store.contract().await?,
            last_indexed_block: last,
            chain_head_block: head,
            lag_blocks: head.map(|h| h.saturating_sub(last.unwrap_or(0))),
            indexer_reported_at: position.reported_at,
            report_valid_for: position.valid_for,
            last_window_error: store.window_error().await?,
            proof_verifier: store.proof_verifier().await?,
        })
    }
}

/// `GET /v1/status` — every chain in the store, as its indexer last left it.
async fn status(State(state): State<AppState>) -> Result<Json<Status>, ApiError> {
    let mut chains = Vec::new();
    for chain_id in state.store.known_chains().await? {
        chains.push(ChainStatus::of(&state.store.chain(chain_id)).await?);
    }
    Ok(Json(Status {
        chains,
        indexer_version: db::INDEXER_VERSION.to_string(),
    }))
}

/// `GET /v1/resolve/handle/{platform}/{handle}?chain=` — the wallet a handle
/// resolves to, the way `resolveHandle` would answer it, on every chain it
/// is bound on or the one named.
async fn resolve_handle(
    State(state): State<AppState>,
    Path((platform, handle)): Path<(String, String)>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<HandleResolution>, ApiError> {
    let platform = parse_platform(&platform)?;
    reject_nul(&handle, "handle")?;
    let chain = filter.parse()?;
    state.synced(chain).await?;

    // Normalize the way the chain did before it keyed the handle. Text the
    // platform could never hold mirrors the contract's `resolveHandle`,
    // which deliberately answers the zero address for it — a 404, not a 400:
    // "nobody holds this", not "you asked wrong".
    let normalized = match platform.normalize_query(&handle) {
        Ok(normalized) => normalized,
        Err(e) => {
            return Err(ApiError::not_found(
                "handle_impossible",
                format!("no handle can exist on this platform for this text: {e}"),
            ));
        }
    };

    let rows = state
        .store
        .resolve_handle(chain, platform.id(), &normalized)
        .await?;
    if rows.is_empty() {
        // Distinguish the contract's UnknownPlatform revert from its
        // zero-address answer: an unwired platform is a different fact than
        // an unclaimed handle.
        if !state.store.platform_wired(chain, platform.id()).await? {
            return Err(ApiError::not_found(
                "platform_not_configured",
                "this platform is not configured on any chain in scope",
            ));
        }
        return Err(ApiError::not_found(
            "handle_not_bound",
            format!("{:?} is not bound", normalized.as_str()),
        ));
    }
    let bindings: Vec<HandleBinding> = rows
        .iter()
        .filter_map(|row| {
            let owner = row.owner.as_deref()?;
            Some(HandleBinding {
                chain_id: row.chain_id,
                handle_node: B256::from_slice(&row.handle_node),
                owner: Address::from_slice(owner),
                observed_at: row.observed_at,
                ceremony_version: row.ceremony_version,
                user_id: row.user_id.clone(),
                id_node: B256::from_slice(&row.id_node),
            })
        })
        .collect();
    if bindings.is_empty() {
        return Err(ApiError::not_found(
            "handle_retired",
            format!(
                "{:?} was retired: its account proved a different handle",
                normalized.as_str()
            ),
        ));
    }

    Ok(Json(HandleResolution {
        platform: platform.known(),
        platform_id: platform.id(),
        handle: normalized.as_str().to_string(),
        bindings,
    }))
}

/// `GET /v1/resolve/id/{platform}/{user_id}?chain=` — the wallet an account
/// id resolves to, the way `resolveId` would answer it, on every chain it is
/// bound on or the one named. The id is matched byte-verbatim, exactly as
/// the chain keys it.
async fn resolve_id(
    State(state): State<AppState>,
    Path((platform, user_id)): Path<(String, String)>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<IdResolution>, ApiError> {
    let platform = parse_platform(&platform)?;
    reject_nul(&user_id, "userId")?;
    let chain = filter.parse()?;
    state.synced(chain).await?;

    let rows = state
        .store
        .resolve_id(chain, platform.id(), &user_id)
        .await?;
    if rows.is_empty() {
        if !state.store.platform_wired(chain, platform.id()).await? {
            return Err(ApiError::not_found(
                "platform_not_configured",
                "this platform is not configured on any chain in scope",
            ));
        }
        return Err(ApiError::not_found(
            "id_not_bound",
            format!("{user_id:?} is not bound"),
        ));
    }

    Ok(Json(IdResolution {
        platform: platform.known(),
        platform_id: platform.id(),
        user_id,
        bindings: rows
            .iter()
            .map(|row| IdBinding {
                chain_id: row.chain_id,
                id_node: B256::from_slice(&row.id_node),
                owner: Address::from_slice(&row.owner),
                observed_at: row.observed_at,
                ceremony_version: row.ceremony_version,
                handle: row.presentable_handle().map(str::to_string),
                handle_node: B256::from_slice(&row.handle_node),
                published: row.displayed(),
            })
            .collect(),
    }))
}

/// `GET /v1/resolve/address/{address}?chain=` — every identity a wallet
/// proved, on every chain the store holds or the one named, with the
/// published flag that mirrors `primaryOf`'s reverse display.
async fn resolve_address(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<AddressResolution>, ApiError> {
    let address = parse_address(&address)?;
    let chain = filter.parse()?;
    state.synced(chain).await?;

    let rows = state.store.identities_of(chain, address).await?;
    let identities = rows
        .iter()
        .map(|row| {
            let platform_id = B256::from_slice(&row.platform_id);
            AddressIdentity {
                chain_id: row.chain_id,
                platform: nodes::Platform::known_of(platform_id),
                platform_id,
                user_id: row.user_id.clone(),
                handle: row.presentable_handle().map(str::to_string),
                handle_node: B256::from_slice(&row.handle_node),
                observed_at: row.observed_at,
                ceremony_version: row.ceremony_version,
                resolves: row.handle_still_owned(),
                published: row.displayed(),
            }
        })
        .collect();

    Ok(Json(AddressResolution {
        address,
        identities,
    }))
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    platform: Option<String>,
    owner: Option<String>,
    chain: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// The page size a search may ask for: at most this many hits per request.
const SEARCH_MAX_LIMIT: i64 = 50;
/// How far into a ranked list a search may page. Deeper pages are a scan the
/// database repeats per request; a client that far in wants a narrower
/// query.
const SEARCH_MAX_OFFSET: i64 = 10_000;

/// `GET /v1/search?q=gre&platform=x&owner=0x…&chain=8453&limit=10&offset=0`
/// — live handles matching a partial query (exact first, then prefix,
/// substring and fuzzy), linked to a wallet, or both, across every chain
/// the store holds or the one named. One of `q` and `owner` is required;
/// `limit` and `offset` page through the ranked list.
async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResults>, ApiError> {
    let chain = ChainFilter {
        chain: params.chain,
    }
    .parse()?;
    state.synced(chain).await?;
    let query = match params.q.as_deref() {
        Some(raw) => {
            reject_nul(raw, "q")?;
            let folded = nodes::fold_search_query(raw);
            if folded.is_empty() {
                return Err(ApiError::bad_request(
                    "invalid_argument",
                    "q must be nonempty",
                ));
            }
            Some(folded)
        }
        None => None,
    };
    let owner = params.owner.as_deref().map(parse_address).transpose()?;
    if query.is_none() && owner.is_none() {
        return Err(ApiError::bad_request(
            "invalid_argument",
            "q or owner is required",
        ));
    }
    let limit = params.limit.unwrap_or(10).clamp(1, SEARCH_MAX_LIMIT);
    let offset = params.offset.unwrap_or(0).clamp(0, SEARCH_MAX_OFFSET);
    let platform_id = params
        .platform
        .as_deref()
        .map(parse_platform)
        .transpose()?
        .map(|p| p.id());

    let rows = state
        .store
        .search_handles(chain, platform_id, owner, query.as_deref(), limit, offset)
        .await?;
    let hits = rows
        .iter()
        .map(|row| {
            let platform_id = B256::from_slice(&row.platform_id);
            SearchHit {
                chain_id: row.chain_id,
                platform: nodes::Platform::known_of(platform_id),
                platform_id,
                handle: row.handle.clone(),
                owner: Address::from_slice(&row.owner),
                user_id: row.user_id.clone(),
                published: row.published,
            }
        })
        .collect();

    Ok(Json(SearchResults {
        query,
        owner,
        limit,
        offset,
        hits,
    }))
}
