//! The read API. Resolution answers exactly what the contract's resolvers
//! answer, from the projections; search, the histories and the escrow's
//! waiting amounts are what the chain cannot list. The answers' shapes live
//! in [`model`]; the handlers here and in the history and escrow modules only
//! translate HTTP to store calls and rows to those shapes.

mod error;
mod escrow;
mod history;
pub mod model;
mod params;

use alloy::primitives::B256;
use axum::{
    extract::State,
    routing::get,
    Json,
    Router,
};
use serde::Deserialize;

use self::{
    error::{
        ApiError,
        ApiPath,
        ApiQuery,
    },
    model::{
        AddressIdentity,
        AddressResolution,
        ChainStatus,
        ErrorCode,
        HandleBinding,
        HandleRef,
        HandleResolution,
        IdBinding,
        IdResolution,
        SearchHit,
        SearchResults,
        Status,
    },
    params::{
        ChainFilter,
        HandleKey,
        PageParams,
        Param,
    },
};
use crate::{
    db::{
        self,
        ChainStatusRow,
        Search,
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

    /// The routes, without middleware.
    ///
    /// No CORS layer here: a `layer` wraps only the routes already on
    /// the router it is called on, so one applied inside this function cannot
    /// cover anything a caller merges afterwards. The binary mounts every route
    /// first and applies CORS once over the whole thing.
    pub fn router(self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/v1/status", get(status))
            .route(
                "/v1/resolve/handle/{platform}/{handle}",
                get(resolve_handle),
            )
            .route(
                "/v1/resolve/handle-node/{handle_node}",
                get(resolve_handle_node),
            )
            .route("/v1/resolve/id-node/{id_node}", get(resolve_id_node))
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
            .with_state(self)
    }

    async fn handle_resolution(
        &self,
        chain: Option<i64>,
        handle_node: B256,
    ) -> Result<Json<HandleResolution>, ApiError> {
        let rows = self.store.resolve_handle(chain, handle_node).await?;
        let Some(first) = rows.first() else {
            return Err(ApiError::not_found(
                ErrorCode::HandleNotBound,
                "nothing is bound under this node",
            ));
        };
        let platform_id = first.platform_id;
        let bindings: Vec<HandleBinding> = rows
            .into_iter()
            .filter_map(|row| {
                Some(HandleBinding {
                    chain_id: row.chain_id,
                    owner: row.owner?,
                    observed_at: row.observed_at,
                    ceremony_version: row.ceremony_version,
                    id_node: row.id_node,
                    handle: row.handle,
                })
            })
            .collect();
        if bindings.is_empty() {
            return Err(ApiError::not_found(
                ErrorCode::HandleRetired,
                "the handle was retired: its account proved a different handle",
            ));
        }
        Ok(Json(HandleResolution {
            platform: nodes::Platform::known_of(platform_id),
            platform_id,
            handle_node,
            bindings,
        }))
    }

    /// `resolveId` takes no platform and never reverts, so a miss is
    /// `id_not_bound` whatever the platform.
    async fn id_resolution(
        &self,
        chain: Option<i64>,
        id_node: B256,
    ) -> Result<Json<IdResolution>, ApiError> {
        let rows = self.store.resolve_id(chain, id_node).await?;
        let Some(first) = rows.first() else {
            return Err(ApiError::not_found(
                ErrorCode::IdNotBound,
                "nothing is bound under this node",
            ));
        };
        let platform_id = first.platform_id;
        Ok(Json(IdResolution {
            platform: nodes::Platform::known_of(platform_id),
            platform_id,
            id_node,
            bindings: rows
                .iter()
                .map(|row| IdBinding {
                    chain_id: row.chain_id,
                    owner: row.owner,
                    observed_at: row.observed_at,
                    ceremony_version: row.ceremony_version,
                    handle: row.presentable_handle().map(str::to_string),
                    handle_node: row.handle_node,
                    published: row.displayed(),
                })
                .collect(),
        }))
    }
}

impl HandleRef {
    /// The handle a request named, completed from `known`: any row about the
    /// node names its platform, and its text once disclosed.
    fn asked(key: &HandleKey, known: Option<&HandleRef>) -> Self {
        let (platform, platform_id) = match key {
            HandleKey::Text { platform, .. } => (Some(*platform), Some(platform.id())),
            HandleKey::Node(_) => (
                known.and_then(|handle| handle.platform),
                known.and_then(|handle| handle.platform_id),
            ),
        };
        Self {
            platform,
            platform_id,
            handle_node: key.node(),
            handle: known.and_then(|handle| handle.handle.clone()),
        }
    }

    /// A stored node with its platform, when known, and its disclosed text.
    fn of(platform_id: Option<B256>, handle_node: B256, handle: Option<String>) -> Self {
        Self {
            platform: platform_id.and_then(nodes::Platform::known_of),
            platform_id,
            handle_node,
            handle,
        }
    }
}

async fn health() -> &'static str {
    "ok"
}

impl ChainStatus {
    /// A chain's status from its row and names.
    fn of(row: ChainStatusRow) -> Self {
        let ChainStatusRow { state, names } = row;
        let position = state.position();
        let head = state.head();
        Self {
            chain_id: state.chain_id,
            names,
            contract: state.contract,
            escrow: state.escrow,
            last_indexed_block: position.cursor,
            chain_head_block: head,
            lag_blocks: head.map(|h| h.saturating_sub(position.cursor.unwrap_or(0))),
            indexer_reported_at: position.reported_at,
            report_valid_for: position.valid_for_seconds(),
            last_window_error: state.window_error,
            proof_verifier: state.proof_verifier,
        }
    }
}

/// `GET /v1/status` — every chain in the store, as its indexer last left it.
async fn status(State(state): State<AppState>) -> Result<Json<Status>, ApiError> {
    let chains = state
        .store
        .chain_statuses()
        .await?
        .into_iter()
        .map(ChainStatus::of)
        .collect();
    Ok(Json(Status {
        chains,
        indexer_version: db::INDEXER_VERSION.to_string(),
    }))
}

/// `GET /v1/resolve/handle/{platform}/{handle}?chain=` — the wallet a handle
/// resolves to, the way `resolveHandle` would answer it, on every chain it
/// is bound on or the one named. The text is hashed here, by the platform's
/// rules, into the node the chain keys.
async fn resolve_handle(
    State(state): State<AppState>,
    ApiPath((platform, handle)): ApiPath<(String, String)>,
    ApiQuery(filter): ApiQuery<ChainFilter>,
) -> Result<Json<HandleResolution>, ApiError> {
    let platform = platform.parse_known_platform()?;
    let chain = filter.chain_id()?;
    state.synced(chain).await?;
    let handle_node = platform.handle_node(&handle)?;
    state.handle_resolution(chain, handle_node).await
}

/// `GET /v1/resolve/handle-node/{handleNode}?chain=` — the same, by node.
async fn resolve_handle_node(
    State(state): State<AppState>,
    ApiPath(handle_node): ApiPath<String>,
    ApiQuery(filter): ApiQuery<ChainFilter>,
) -> Result<Json<HandleResolution>, ApiError> {
    let handle_node = handle_node.parse_node()?;
    let chain = filter.chain_id()?;
    state.synced(chain).await?;
    state.handle_resolution(chain, handle_node).await
}

/// `GET /v1/resolve/id-node/{idNode}?chain=` — the wallet an id node resolves
/// to, the way `resolveId` would answer it, on every chain it is bound on or
/// the one named. The client hashes the id; no plaintext id reaches here.
async fn resolve_id_node(
    State(state): State<AppState>,
    ApiPath(id_node): ApiPath<String>,
    ApiQuery(filter): ApiQuery<ChainFilter>,
) -> Result<Json<IdResolution>, ApiError> {
    let id_node = id_node.parse_node()?;
    let chain = filter.chain_id()?;
    state.synced(chain).await?;
    state.id_resolution(chain, id_node).await
}

/// `GET /v1/resolve/address/{address}?chain=` — every identity a wallet
/// proved, on every chain the store holds or the one named, with the
/// published flag that marks the one whose handle `primaryHandleOf` answers.
async fn resolve_address(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
    ApiQuery(filter): ApiQuery<ChainFilter>,
) -> Result<Json<AddressResolution>, ApiError> {
    let address = address.parse_address()?;
    let chain = filter.chain_id()?;
    state.synced(chain).await?;

    let rows = state.store.identities_of(chain, address).await?;
    let identities = rows
        .iter()
        .map(|row| AddressIdentity {
            chain_id: row.chain_id,
            platform: nodes::Platform::known_of(row.platform_id),
            platform_id: row.platform_id,
            id_node: row.id_node,
            handle: row.presentable_handle().map(str::to_string),
            handle_node: row.handle_node,
            observed_at: row.observed_at,
            ceremony_version: row.ceremony_version,
            resolves: row.handle_still_owned(),
            published: row.displayed(),
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
/// A search page holds this many hits when the request names no `limit`.
const SEARCH_DEFAULT_LIMIT: i64 = 10;
/// How far into a ranked list a search may page. Deeper pages are a scan the
/// database repeats per request; a client that far in wants a narrower
/// query.
const SEARCH_MAX_OFFSET: i64 = 10_000;

/// `GET /v1/search?q=gre&platform=x&owner=0x…&chain=8453&limit=10&offset=0`
/// — live, disclosed handles matching a partial query (exact first, then prefix,
/// substring and fuzzy), linked to a wallet, or both, across every chain
/// the store holds or the one named. One of `q` and `owner` is required;
/// `limit` and `offset` page through the ranked list.
async fn search(
    State(state): State<AppState>,
    ApiQuery(params): ApiQuery<SearchParams>,
) -> Result<Json<SearchResults>, ApiError> {
    let chain = ChainFilter {
        chain: params.chain,
    }
    .chain_id()?;
    let query = match params.q.as_deref() {
        Some(raw) => {
            raw.refuse_nul("q")?;
            let folded = nodes::fold_search_query(raw);
            if folded.is_empty() {
                return Err(ApiError::bad_request(
                    ErrorCode::InvalidArgument,
                    "q must be nonempty",
                ));
            }
            Some(folded)
        }
        None => None,
    };
    let owner = params
        .owner
        .as_deref()
        .map(str::parse_address)
        .transpose()?;
    if query.is_none() && owner.is_none() {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidArgument,
            "q or owner is required",
        ));
    }
    let limit = params
        .limit
        .as_deref()
        .map(|limit| limit.parse_count("limit"))
        .transpose()?
        .unwrap_or(SEARCH_DEFAULT_LIMIT)
        .clamp(1, SEARCH_MAX_LIMIT);
    let offset = params
        .offset
        .as_deref()
        .map(|offset| offset.parse_count("offset"))
        .transpose()?
        .unwrap_or(0)
        .clamp(0, SEARCH_MAX_OFFSET);
    let platform_id = params
        .platform
        .as_deref()
        .map(str::parse_platform)
        .transpose()?
        .map(|p| p.id());
    state.synced(chain).await?;

    let rows = state
        .store
        .search_handles(&Search {
            chain,
            platform_id,
            owner,
            folded_query: query.as_deref(),
            limit,
            offset,
        })
        .await?;
    let hits = rows
        .iter()
        .map(|row| SearchHit {
            chain_id: row.chain_id,
            platform: nodes::Platform::known_of(row.platform_id),
            platform_id: row.platform_id,
            handle: row.handle.clone(),
            handle_node: row.handle_node,
            owner: row.owner,
            id_node: row.id_node,
            published: row.published,
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
    use alloy::primitives::Address;
    use axum::{
        body::Body,
        http::{
            Request,
            StatusCode,
        },
    };
    use http_body_util::BodyExt;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use super::*;
    use crate::api::model::ErrorBody;

    /// The status and code a route refuses `path` with. Every refusal here
    /// comes before the store is read, so the pool never connects.
    async fn refusal(path: &str) -> (StatusCode, ErrorCode) {
        let pool =
            PgPool::connect_lazy("postgres://localhost/unread").expect("lazy pool");
        let response = AppState::new(Store::new(pool))
            .router()
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
        let bad = |code: ErrorCode| (StatusCode::BAD_REQUEST, code);
        let cases = [
            (
                "/v1/escrow/node/0x1234".to_string(),
                bad(ErrorCode::InvalidNode),
            ),
            (
                "/v1/history/node/alice".to_string(),
                bad(ErrorCode::InvalidNode),
            ),
            (
                "/v1/escrow/claimable/0xnope".to_string(),
                bad(ErrorCode::InvalidAddress),
            ),
            (
                "/v1/escrow/unclaimed?token=usdc".to_string(),
                bad(ErrorCode::InvalidAddress),
            ),
            (
                "/v1/escrow/handle/myspace/tom".to_string(),
                bad(ErrorCode::InvalidPlatform),
            ),
            (
                "/v1/escrow/unclaimed?limit=ten".to_string(),
                bad(ErrorCode::InvalidArgument),
            ),
            (
                "/v1/history/handle/x/nobody?limit=1.5".to_string(),
                bad(ErrorCode::InvalidArgument),
            ),
            (
                "/v1/escrow/unclaimed?chain=base".to_string(),
                bad(ErrorCode::InvalidChain),
            ),
            (
                "/v1/escrow/unclaimed?before=nope".to_string(),
                bad(ErrorCode::InvalidCursor),
            ),
            (
                format!("/v1/history/address/{address}?before=yesterday"),
                bad(ErrorCode::InvalidCursor),
            ),
            (
                format!("/v1/escrow/refundable/{address}?before=1-2"),
                bad(ErrorCode::InvalidCursor),
            ),
            (
                "/v1/history/address/%FF".to_string(),
                bad(ErrorCode::InvalidPath),
            ),
            (
                "/v1/resolve/address/%FF".to_string(),
                bad(ErrorCode::InvalidPath),
            ),
            (
                "/v1/search?q=al&q=bo".to_string(),
                bad(ErrorCode::InvalidQuery),
            ),
            (
                "/v1/search?q=ali&limit=ten".to_string(),
                bad(ErrorCode::InvalidArgument),
            ),
            (
                "/v1/search?q=ali&offset=1.5".to_string(),
                bad(ErrorCode::InvalidArgument),
            ),
            (
                "/v1/resolve/handle/x/alice?chain=1&chain=2".to_string(),
                bad(ErrorCode::InvalidQuery),
            ),
            (
                format!("/v1/escrow/claimable/{address}?chain=1&chain=2"),
                bad(ErrorCode::InvalidQuery),
            ),
        ];
        for (path, expected) in cases {
            assert_eq!(refusal(&path).await, expected, "{path}");
        }
    }
}
