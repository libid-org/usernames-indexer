//! `GET /v1/referral/...`: referral codes, the short form of a bound handle
//! node a share link carries in place of the handle's text.

use alloy::primitives::B256;
use axum::{
    extract::State,
    Json,
};

use super::{
    model::{
        ErrorCode,
        Referrer,
    },
    ApiError,
    ApiPath,
    AppState,
    Param,
};
use crate::{
    db::ReferrerRow,
    nodes,
    referral::Code,
};

impl Referrer {
    /// The referrer `code` names, from its node's rows on every chain that
    /// bound it; `None` without one.
    fn of(code: Code, handle_node: B256, rows: Vec<ReferrerRow>) -> Option<Self> {
        let platform_id = rows.first()?.platform_id;
        let primary_handle = rows.iter().find_map(|row| row.published_handle.clone());
        let chains = rows
            .iter()
            .filter(|row| row.owner.is_some())
            .map(|row| row.chain_id)
            .collect();
        Some(Self {
            code,
            platform: nodes::Platform::known_of(platform_id),
            platform_id,
            handle_node,
            primary_handle,
            chains,
        })
    }
}

/// `GET /v1/referral/code/{code}` — the handle a referral code names: of the
/// nodes bound on any chain that its bytes start, the one bound first.
pub(crate) async fn code(
    State(state): State<AppState>,
    ApiPath(code): ApiPath<String>,
) -> Result<Json<Referrer>, ApiError> {
    let code: Code = code.parse()?;
    state.synced(None).await?;

    let Some(node) = state.store.code_owner(&code).await? else {
        return Err(ApiError::code_not_issued());
    };
    state.referrer(code, node).await
}

/// `GET /v1/referral/node/{node}/code` — the shortest code that names a
/// handle node bound on any chain, and the referrer it shows.
pub(crate) async fn node_code(
    State(state): State<AppState>,
    ApiPath(node): ApiPath<String>,
) -> Result<Json<Referrer>, ApiError> {
    let node = node.parse_node()?;
    state.synced(None).await?;

    let Some(code) = state.store.code_of(node).await? else {
        return Err(ApiError::not_found(
            ErrorCode::HandleNotBound,
            "no chain has bound this node",
        ));
    };
    state.referrer(code, node).await
}

impl AppState {
    /// A replay that clears the node's only chain between the two reads
    /// leaves the code naming nothing.
    async fn referrer(&self, code: Code, node: B256) -> Result<Json<Referrer>, ApiError> {
        let rows = self.store.referrer(node).await?;
        Referrer::of(code, node, rows)
            .map(Json)
            .ok_or_else(ApiError::code_not_issued)
    }
}
