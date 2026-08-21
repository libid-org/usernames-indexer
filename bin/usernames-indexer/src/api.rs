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
use sqlx::{
    PgPool,
    Row,
};

use crate::{
    db,
    nodes,
};

/// What every handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The shared read pool.
    pub pool: PgPool,
    /// The chain this deployment serves.
    pub chain_id: i64,
    /// The watched contract, echoed in `/v1/status`.
    pub contract: Address,
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

/// A platform named by its short key ('x') or its 0x-hex 32-byte id. The key
/// form also carries this build's normalization rules; the hex form indexes
/// fine but string queries run un-normalized.
struct PlatformRef {
    id: B256,
    key: Option<&'static str>,
}

fn parse_platform(raw: &str) -> Result<PlatformRef, ApiError> {
    if let Some(id) = nodes::platform_id_for_key(raw) {
        return Ok(PlatformRef {
            id,
            key: Some(nodes::KNOWN_PLATFORMS[&id]),
        });
    }
    if let Ok(id) = B256::from_str(raw) {
        return Ok(PlatformRef {
            id,
            key: nodes::KNOWN_PLATFORMS.get(&id).copied(),
        });
    }
    Err(ApiError::bad_request(format!(
        "unknown platform {raw:?}: use x, github, google, or a 0x-hex platform id"
    )))
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
    match db::get_cursor(&state.pool, state.chain_id).await? {
        Some(_) => Ok(()),
        None => Err(ApiError::not_synced()),
    }
}

/// Whether the chain ever configured this platform — the difference between
/// the contract's `UnknownPlatform` revert and its zero-address answer.
async fn platform_wired(
    state: &AppState,
    platform: &PlatformRef,
) -> Result<bool, ApiError> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM names.platforms WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(state.chain_id)
    .bind(platform.id.as_slice())
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.is_some())
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
    let last = db::get_cursor(&state.pool, state.chain_id).await?;
    let head = db::get_chain_head(&state.pool, state.chain_id).await?;
    Ok(Json(Status {
        chain_id: state.chain_id,
        contract: state.contract.to_string(),
        last_indexed_block: last,
        chain_head_block: head,
        lag_blocks: head.map(|h| h.saturating_sub(last.unwrap_or(0))),
        last_window_error: db::get_window_error(&state.pool, state.chain_id).await?,
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
    id_node: Option<String>,
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
    // "nobody holds this", not "you asked wrong". A platform this build has
    // no rules for is queried as given — its handles were still stored
    // normalized, the caller just has to supply that form.
    let normalized = match platform.key.and_then(libid_identity::Rules::for_platform) {
        Some(rules) => match libid_identity::normalize(&handle, rules) {
            Ok(normalized) => normalized,
            Err(e) => {
                return Err(ApiError::not_found(format!(
                    "no handle can exist on this platform for this text: {e}"
                )));
            }
        },
        None => handle,
    };

    let row = sqlx::query(
        r#"SELECT h.platform_id, h.handle, h.handle_node, h.owner, h.observed_at,
                  h.version, h.id_node,
                  i.user_id, i.owner AS id_owner
           FROM names.handles h
           LEFT JOIN names.ids i
             ON i.chain_id = h.chain_id AND i.id_node = h.id_node
           WHERE h.chain_id = $1 AND h.platform_id = $2 AND h.handle = $3"#,
    )
    .bind(state.chain_id)
    .bind(platform.id.as_slice())
    .bind(&normalized)
    .fetch_optional(&state.pool)
    .await?;

    let Some(row) = row else {
        // Distinguish the contract's UnknownPlatform revert from its
        // zero-address answer: an unwired platform is a different fact than
        // an unclaimed handle.
        if !platform_wired(&state, &platform).await? {
            return Err(ApiError::not_found(
                "this platform is not configured on this chain",
            ));
        }
        return Err(ApiError::not_found(format!("{normalized:?} is not bound")));
    };
    let owner: Option<Vec<u8>> = row.try_get("owner")?;
    let Some(owner) = owner else {
        return Err(ApiError::not_found(format!(
            "{normalized:?} was retired: its account proved a different handle"
        )));
    };
    let id_owner: Option<Vec<u8>> = row.try_get("id_owner")?;
    let id_node: Option<Vec<u8>> = row.try_get("id_node")?;

    Ok(Json(HandleResolution {
        platform: platform.key,
        platform_id: platform.id.to_string(),
        handle: row.try_get("handle")?,
        handle_node: b256_from_db(row.try_get::<Vec<u8>, _>("handle_node")?.as_slice()),
        owner: address_from_db(&owner),
        observed_at: row.try_get("observed_at")?,
        version: row.try_get("version")?,
        user_id: row.try_get("user_id")?,
        id_node: id_node.as_deref().map(b256_from_db),
        id_agrees: id_owner.as_deref() == Some(owner.as_slice()),
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

    let row = sqlx::query(
        r#"SELECT i.user_id, i.id_node, i.owner, i.observed_at, i.version,
                  i.handle_node,
                  h.handle, h.owner AS handle_owner, h.id_node AS handle_id_node,
                  (p.handle IS NOT NULL) AS published
           FROM names.ids i
           LEFT JOIN names.handles h
             ON h.chain_id = i.chain_id AND h.handle_node = i.handle_node
           LEFT JOIN names.published p
             ON p.chain_id = i.chain_id AND p.owner = i.owner
                AND p.platform_id = i.platform_id AND p.handle = h.handle
           WHERE i.chain_id = $1 AND i.platform_id = $2 AND i.user_id = $3"#,
    )
    .bind(state.chain_id)
    .bind(platform.id.as_slice())
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await?;

    let Some(row) = row else {
        if !platform_wired(&state, &platform).await? {
            return Err(ApiError::not_found(
                "this platform is not configured on this chain",
            ));
        }
        return Err(ApiError::not_found(format!("{user_id:?} is not bound")));
    };

    let owner: Vec<u8> = row.try_get("owner")?;
    let id_node: Vec<u8> = row.try_get("id_node")?;
    let handle_owner: Option<Vec<u8>> = row.try_get("handle_owner")?;
    let handle_id_node: Option<Vec<u8>> = row.try_get("handle_id_node")?;

    // The handle is still the account's when the node points back at this id
    // and still resolves to the same wallet. Otherwise the account renamed or
    // was overtaken, and showing the stale string would mis-route a payment.
    let still_theirs = handle_owner.as_deref() == Some(owner.as_slice())
        && handle_id_node.as_deref() == Some(id_node.as_slice());
    let handle: Option<String> = row.try_get("handle")?;

    Ok(Json(IdResolution {
        platform: platform.key,
        platform_id: platform.id.to_string(),
        user_id: row.try_get("user_id")?,
        id_node: b256_from_db(&id_node),
        owner: address_from_db(&owner),
        observed_at: row.try_get("observed_at")?,
        version: row.try_get("version")?,
        handle: if still_theirs { handle } else { None },
        handle_node: b256_from_db(row.try_get::<Vec<u8>, _>("handle_node")?.as_slice()),
        published: row
            .try_get::<Option<bool>, _>("published")?
            .unwrap_or(false)
            && still_theirs,
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

    let rows = sqlx::query(
        r#"SELECT i.platform_id, i.user_id, i.observed_at, i.version, i.handle_node,
                  h.handle, h.owner AS handle_owner, h.id_node AS handle_id_node,
                  i.id_node,
                  (p.handle IS NOT NULL) AS published
           FROM names.ids i
           LEFT JOIN names.handles h
             ON h.chain_id = i.chain_id AND h.handle_node = i.handle_node
           LEFT JOIN names.published p
             ON p.chain_id = i.chain_id AND p.owner = i.owner
                AND p.platform_id = i.platform_id AND p.handle = h.handle
           WHERE i.chain_id = $1 AND i.owner = $2
           ORDER BY i.platform_id, i.user_id"#,
    )
    .bind(state.chain_id)
    .bind(address.as_slice())
    .fetch_all(&state.pool)
    .await?;

    let mut identities = Vec::with_capacity(rows.len());
    for row in rows {
        let platform_id =
            B256::from_slice(row.try_get::<Vec<u8>, _>("platform_id")?.as_slice());
        let id_node: Vec<u8> = row.try_get("id_node")?;
        let handle_owner: Option<Vec<u8>> = row.try_get("handle_owner")?;
        let handle_id_node: Option<Vec<u8>> = row.try_get("handle_id_node")?;
        let resolves = handle_owner.as_deref() == Some(address.as_slice())
            && handle_id_node.as_deref() == Some(id_node.as_slice());
        let handle: Option<String> = row.try_get("handle")?;
        identities.push(AddressIdentity {
            platform: nodes::KNOWN_PLATFORMS.get(&platform_id).copied(),
            platform_id: platform_id.to_string(),
            user_id: row.try_get("user_id")?,
            handle: if resolves { handle } else { None },
            handle_node: b256_from_db(
                row.try_get::<Vec<u8>, _>("handle_node")?.as_slice(),
            ),
            observed_at: row.try_get("observed_at")?,
            version: row.try_get("version")?,
            resolves,
            published: row
                .try_get::<Option<bool>, _>("published")?
                .unwrap_or(false)
                && resolves,
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

/// Fold a search query the way normalization would, without refusing partial
/// input: trim spaces, strip one leading `@`, lowercase A-Z. A partial handle
/// cannot pass shape checks, so full normalization is deliberately not run.
fn fold_query(raw: &str) -> String {
    let trimmed = raw.trim_matches(' ');
    let stripped = trimmed.strip_prefix('@').unwrap_or(trimmed);
    stripped.to_ascii_lowercase()
}

/// Escape LIKE metacharacters so the query text matches itself.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `GET /v1/search?q=gre&platform=x&limit=10` — matching variants for a
/// partial handle, exact first, then prefix, then substring and fuzzy.
async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResults>, ApiError> {
    reject_nul(&params.q, "q")?;
    ensure_synced(&state).await?;
    let query = fold_query(&params.q);
    if query.is_empty() {
        return Err(ApiError::bad_request("q must be nonempty"));
    }
    let limit = params.limit.unwrap_or(10).clamp(1, 50);
    let platform_id = params
        .platform
        .as_deref()
        .map(parse_platform)
        .transpose()?
        .map(|p| p.id);
    let like = escape_like(&query);

    let rows = sqlx::query(
        r#"SELECT h.platform_id, h.handle, h.owner, i.user_id,
                  (p.handle IS NOT NULL) AS published
           FROM names.handles h
           LEFT JOIN names.ids i
             ON i.chain_id = h.chain_id AND i.id_node = h.id_node
           LEFT JOIN names.published p
             ON p.chain_id = h.chain_id AND p.owner = h.owner
                AND p.platform_id = h.platform_id AND p.handle = h.handle
           WHERE h.chain_id = $1
             AND h.owner IS NOT NULL
             AND ($2::bytea IS NULL OR h.platform_id = $2)
             AND (h.handle LIKE '%' || $3 || '%' ESCAPE '\'
                  OR h.handle % $4)
           ORDER BY (h.handle = $4) DESC,
                    (h.handle LIKE $3 || '%' ESCAPE '\') DESC,
                    (h.handle LIKE '%' || $3 || '%' ESCAPE '\') DESC,
                    similarity(h.handle, $4) DESC,
                    h.handle ASC
           LIMIT $5"#,
    )
    .bind(state.chain_id)
    .bind(platform_id.as_ref().map(|p| p.as_slice().to_vec()))
    .bind(&like)
    .bind(&query)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let platform_id =
            B256::from_slice(row.try_get::<Vec<u8>, _>("platform_id")?.as_slice());
        hits.push(SearchHit {
            platform: nodes::KNOWN_PLATFORMS.get(&platform_id).copied(),
            platform_id: platform_id.to_string(),
            handle: row.try_get("handle")?,
            owner: address_from_db(row.try_get::<Vec<u8>, _>("owner")?.as_slice()),
            user_id: row.try_get("user_id")?,
            published: row
                .try_get::<Option<bool>, _>("published")?
                .unwrap_or(false),
        });
    }

    Ok(Json(SearchResults { query, hits }))
}
