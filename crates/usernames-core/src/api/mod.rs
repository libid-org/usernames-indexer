//! The read API. Resolution answers exactly what the contract's resolvers
//! answer, from the projections; search, the histories and the escrow's
//! waiting amounts are what the chain cannot list. The answers' shapes live
//! in [`model`]; the handlers here and in [`history`] and [`escrow`] only
//! translate HTTP to store calls and rows to those shapes.

mod escrow;
mod history;
pub mod model;

use std::{
    fmt,
    str::FromStr,
};

use alloy::primitives::{
    Address,
    B256,
};
use axum::{
    extract::{
        rejection::{
            PathRejection,
            QueryRejection,
        },
        FromRequestParts,
        Path,
        Query,
        State,
    },
    http::{
        request::Parts,
        StatusCode,
    },
    response::{
        IntoResponse,
        Response,
    },
    routing::get,
    Json,
    Router,
};
use serde::{
    de::DeserializeOwned,
    Deserialize,
};

use self::model::*;
use crate::{
    db::{
        self,
        ChainStore,
        InvalidCursor,
        Page,
        Store,
    },
    nodes::{
        self,
        NormalizedHandle,
    },
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
        .route("/v1/history/address/{address}", get(history::address))
        .route(
            "/v1/history/handle/{platform}/{handle}",
            get(history::handle),
        )
        .route("/v1/history/node/{node}", get(history::node))
        .route("/v1/escrow/claimable/{address}", get(escrow::claimable))
        .route("/v1/escrow/refundable/{address}", get(escrow::refundable))
        .route("/v1/escrow/handle/{platform}/{handle}", get(escrow::handle))
        .route("/v1/escrow/node/{node}", get(escrow::node))
        .route("/v1/escrow/unclaimed", get(escrow::unclaimed))
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

    /// A stored value this build cannot read back. Logged with `what` where
    /// it is found; the client gets the same opaque 500 as a database error.
    fn internal(what: impl fmt::Display) -> Self {
        tracing::error!(%what, "unreadable stored value");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "internal error".into(),
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

/// Text the platform could never hold, refused the way the contract's
/// `resolveHandle` answers it — the zero address, so a 404 and not a 400:
/// "nobody holds this", not "you asked wrong".
impl From<libid_identity::HandleError> for ApiError {
    fn from(e: libid_identity::HandleError) -> Self {
        Self::not_found(
            "handle_impossible",
            format!("no handle can exist on this platform for this text: {e}"),
        )
    }
}

/// A path segment axum could not take apart, such as invalid UTF-8: a 400 in
/// the envelope, or the 500 of a route that names no such segment.
impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        if rejection.status().is_server_error() {
            Self::internal(rejection.body_text())
        } else {
            Self::bad_request("invalid_path", rejection.body_text())
        }
    }
}

/// A query string axum could not take apart, such as a repeated key.
impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::bad_request("invalid_query", rejection.body_text())
    }
}

impl From<InvalidCursor> for ApiError {
    fn from(e: InvalidCursor) -> Self {
        Self::bad_request("invalid_cursor", e.to_string())
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

/// `Path`, refused in the API's envelope instead of axum's plain text.
struct ApiPath<T>(T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Path(value) = Path::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}

/// `Query`, refused in the API's envelope instead of axum's plain text.
struct ApiQuery<T>(T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Query(value) = Query::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}

/// `?chain=8453&before=<next>&limit=20`: one page of a list, the cursor the
/// one the previous page handed out.
#[derive(Deserialize)]
struct PageParams {
    chain: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

impl PageParams {
    fn page<C: FromStr<Err = InvalidCursor>>(self) -> Result<Page<C>, ApiError> {
        Ok(Page {
            chain: ChainFilter { chain: self.chain }.parse()?,
            before: self.before.as_deref().map(str::parse).transpose()?,
            limit: parse_count(self.limit.as_deref(), "limit")?
                .unwrap_or(PAGE_DEFAULT_LIMIT)
                .clamp(1, PAGE_MAX_LIMIT),
        })
    }
}

/// A handle as a request names it: by its text on a platform, or by its node
/// alone, for one somebody deposited for and nobody has bound.
enum HandleKey {
    Text {
        platform: nodes::Platform,
        handle: NormalizedHandle,
    },
    Node(B256),
}

impl HandleKey {
    fn of_text(platform: &str, handle: &str) -> Result<Self, ApiError> {
        let platform = parse_platform(platform)?;
        reject_nul(handle, "handle")?;
        let handle = platform.normalize_query(handle)?;
        Ok(Self::Text { platform, handle })
    }

    fn of_node(node: &str) -> Result<Self, ApiError> {
        parse_node(node).map(Self::Node)
    }

    fn node(&self) -> B256 {
        match self {
            Self::Text { platform, handle } => nodes::handle_node(platform.id(), handle),
            Self::Node(node) => *node,
        }
    }
}

impl HandleQuery {
    /// The handle a request named, completed from `known` when it named only
    /// the node: any row about the node names its platform, and its text
    /// once a bind has carried it.
    fn of(key: &HandleKey, known: Option<&HandleRef>) -> Self {
        match key {
            HandleKey::Text { platform, handle } => Self {
                platform: platform.known(),
                platform_id: Some(platform.id()),
                handle_node: key.node(),
                handle: Some(handle.as_str().to_string()),
            },
            HandleKey::Node(node) => Self {
                platform: known.and_then(|handle| handle.platform),
                platform_id: known.map(|handle| handle.platform_id),
                handle_node: *node,
                handle: known.and_then(|handle| handle.handle.clone()),
            },
        }
    }
}

impl HandleRef {
    /// A stored node with its platform and the text a bind carried, if any.
    fn of(platform_id: B256, handle_node: B256, handle: Option<String>) -> Self {
        Self {
            platform: nodes::Platform::known_of(platform_id),
            platform_id,
            handle_node,
            handle,
        }
    }
}

fn parse_platform(raw: &str) -> Result<nodes::Platform, ApiError> {
    raw.parse::<nodes::Platform>()
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

/// A count from the query string, refused in the API's envelope: typed as a
/// number in the extractor, `limit=ten` would get axum's plain-text 400.
fn parse_count(raw: Option<&str>, name: &str) -> Result<Option<i64>, ApiError> {
    raw.map(|raw| {
        raw.parse::<i64>().map_err(|_| {
            ApiError::bad_request(
                "invalid_argument",
                format!("{name} must be an integer, not {raw:?}"),
            )
        })
    })
    .transpose()
}

fn parse_node(raw: &str) -> Result<B256, ApiError> {
    B256::from_str(raw).map_err(|_| {
        ApiError::bad_request(
            "invalid_node",
            format!("{raw:?} is not a 0x-hex 32-byte handle node"),
        )
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
            names: store.chain_names().await?,
            contract: store.contract().await?,
            escrow: store.escrow().await?,
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

    // Normalize the way the chain did before it keyed the handle.
    let normalized = platform.normalize_query(&handle)?;

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
/// published flag that mirrors `publishedHandleOf`'s reverse display.
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
    limit: Option<String>,
    offset: Option<String>,
}

/// The page size a search may ask for: at most this many hits per request.
const SEARCH_MAX_LIMIT: i64 = 50;
/// How far into a ranked list a search may page. Deeper pages are a scan the
/// database repeats per request; a client that far in wants a narrower
/// query.
const SEARCH_MAX_OFFSET: i64 = 10_000;
/// A list page holds this many entries when the request names no `limit`.
const PAGE_DEFAULT_LIMIT: i64 = 20;
/// The most entries one list page holds.
const PAGE_MAX_LIMIT: i64 = 100;

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
    let limit = parse_count(params.limit.as_deref(), "limit")?
        .unwrap_or(10)
        .clamp(1, SEARCH_MAX_LIMIT);
    let offset = parse_count(params.offset.as_deref(), "offset")?
        .unwrap_or(0)
        .clamp(0, SEARCH_MAX_OFFSET);
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

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::Request,
    };
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use super::*;

    /// The status and code a route refuses `path` with. Every refusal here
    /// comes before the store is read, so the pool never connects.
    async fn refusal(path: &str) -> (StatusCode, String) {
        let pool =
            PgPool::connect_lazy("postgres://localhost/unread").expect("lazy pool");
        let response = router(AppState::new(Store::new(pool)))
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let refusal: ErrorBody =
            serde_json::from_slice(&body).expect("the error envelope");
        (status, refusal.error.code)
    }

    #[tokio::test]
    async fn a_malformed_list_request_is_refused_in_the_envelope() {
        let address = Address::repeat_byte(0xA1);
        let bad = |code: &str| (StatusCode::BAD_REQUEST, code.to_string());
        let cases = [
            ("/v1/escrow/node/0x1234".to_string(), bad("invalid_node")),
            ("/v1/history/node/alice".to_string(), bad("invalid_node")),
            (
                "/v1/escrow/claimable/0xnope".to_string(),
                bad("invalid_address"),
            ),
            (
                "/v1/escrow/unclaimed?token=usdc".to_string(),
                bad("invalid_address"),
            ),
            (
                "/v1/escrow/handle/myspace/tom".to_string(),
                bad("invalid_platform"),
            ),
            (
                "/v1/history/handle/x/no%20spaces".to_string(),
                (StatusCode::NOT_FOUND, "handle_impossible".to_string()),
            ),
            (
                "/v1/escrow/unclaimed?limit=ten".to_string(),
                bad("invalid_argument"),
            ),
            (
                "/v1/history/handle/x/nobody?limit=1.5".to_string(),
                bad("invalid_argument"),
            ),
            (
                "/v1/escrow/unclaimed?chain=base".to_string(),
                bad("invalid_chain"),
            ),
            (
                "/v1/escrow/unclaimed?before=nope".to_string(),
                bad("invalid_cursor"),
            ),
            (
                format!("/v1/history/address/{address}?before=yesterday"),
                bad("invalid_cursor"),
            ),
            (
                format!("/v1/escrow/refundable/{address}?before=1-2"),
                bad("invalid_cursor"),
            ),
            ("/v1/history/address/%FF".to_string(), bad("invalid_path")),
            (
                format!("/v1/escrow/claimable/{address}?chain=1&chain=2"),
                bad("invalid_query"),
            ),
        ];
        for (path, expected) in cases {
            assert_eq!(refusal(&path).await, expected, "{path}");
        }
    }
}
