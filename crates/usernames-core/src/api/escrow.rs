//! `GET /v1/escrow/...`: what the escrow holds — for an address to claim or
//! refund, against one handle, and every slot still unclaimed.

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
    Json,
};
use serde::Deserialize;

use super::{
    model::{
        AddressBalances,
        EscrowAmount,
        HandleBalances,
        HandleQuery,
        HandleRef,
        Unclaimed,
    },
    parse_address,
    parse_count,
    parse_node,
    parse_platform,
    reject_nul,
    ApiError,
    AppState,
    ChainFilter,
    MAX_OFFSET,
    PAGE_DEFAULT_LIMIT,
    PAGE_MAX_LIMIT,
};
use crate::{
    db::{
        EscrowSlotRow,
        UnclaimedPage,
    },
    nodes,
};

impl EscrowAmount {
    /// The amount a stored slot holds. The books are this build's own
    /// writing, so a value that is not a `uint256` is an internal error.
    fn of(row: EscrowSlotRow) -> Result<Self, ApiError> {
        let (Some(amount), Some(round)) = (row.amount(), row.round()) else {
            return Err(ApiError::internal(format_args!(
                "escrow amount {:?} round {:?} on chain {}",
                row.amount, row.round, row.chain_id
            )));
        };
        let platform_id = B256::from_slice(&row.platform_id);
        Ok(Self {
            chain_id: row.chain_id,
            holder: row.holder_address(),
            token: Address::from_slice(&row.token),
            amount,
            round,
            handle: HandleRef {
                platform: nodes::Platform::known_of(platform_id),
                platform_id,
                handle_node: B256::from_slice(&row.handle_node),
                handle: row.handle,
            },
        })
    }

    fn all_of(rows: Vec<EscrowSlotRow>) -> Result<Vec<Self>, ApiError> {
        rows.into_iter().map(Self::of).collect()
    }
}

/// `GET /v1/escrow/address/{address}?chain=` — what is waiting for an address:
/// everything held for the handles it holds now, which `claim` pays it, and
/// what deposits naming it as `refundTo` booked that nobody has claimed yet,
/// which `refund` pays back.
pub(super) async fn address(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<AddressBalances>, ApiError> {
    let address = parse_address(&address)?;
    let chain = filter.parse()?;
    state.synced(chain).await?;

    Ok(Json(AddressBalances {
        address,
        claimable: EscrowAmount::all_of(state.store.claimable(chain, address).await?)?,
        refundable: EscrowAmount::all_of(state.store.refundable(chain, address).await?)?,
    }))
}

/// `GET /v1/escrow/handle/{platform}/{handle}?chain=` — what a handle holds,
/// token by token, and who may claim it.
pub(super) async fn handle(
    State(state): State<AppState>,
    Path((platform, handle)): Path<(String, String)>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<HandleBalances>, ApiError> {
    let platform = parse_platform(&platform)?;
    reject_nul(&handle, "handle")?;
    let chain = filter.parse()?;
    state.synced(chain).await?;
    let normalized = platform.normalize_query(&handle)?;

    let handle_node = nodes::handle_node(platform.id(), &normalized);
    Ok(Json(HandleBalances {
        handle: HandleQuery {
            platform: platform.known(),
            platform_id: Some(platform.id()),
            handle_node,
            handle: Some(normalized.as_str().to_string()),
        },
        held: EscrowAmount::all_of(state.store.escrowed_for(chain, handle_node).await?)?,
    }))
}

/// `GET /v1/escrow/node/{node}?chain=` — the same, for a handle known only by
/// its node.
pub(super) async fn node(
    State(state): State<AppState>,
    Path(node): Path<String>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<HandleBalances>, ApiError> {
    let handle_node = parse_node(&node)?;
    let chain = filter.parse()?;
    state.synced(chain).await?;

    let held = EscrowAmount::all_of(state.store.escrowed_for(chain, handle_node).await?)?;
    let known = held.first().map(|amount| &amount.handle);
    Ok(Json(HandleBalances {
        handle: HandleQuery {
            platform: known.and_then(|handle| handle.platform),
            platform_id: known.map(|handle| handle.platform_id),
            handle_node,
            handle: known.and_then(|handle| handle.handle.clone()),
        },
        held,
    }))
}

/// `?token=0x…&platform=x&chain=8453&limit=20&offset=0`.
#[derive(Deserialize)]
pub(super) struct UnclaimedParams {
    token: Option<String>,
    platform: Option<String>,
    chain: Option<String>,
    limit: Option<String>,
    offset: Option<String>,
}

impl UnclaimedParams {
    fn page(self) -> Result<UnclaimedPage, ApiError> {
        Ok(UnclaimedPage {
            chain: ChainFilter { chain: self.chain }.parse()?,
            token: self.token.as_deref().map(parse_address).transpose()?,
            platform_id: self
                .platform
                .as_deref()
                .map(parse_platform)
                .transpose()?
                .map(|platform| platform.id()),
            limit: parse_count(self.limit.as_deref(), "limit")?
                .unwrap_or(PAGE_DEFAULT_LIMIT)
                .clamp(1, PAGE_MAX_LIMIT),
            offset: parse_count(self.offset.as_deref(), "offset")?
                .unwrap_or(0)
                .clamp(0, MAX_OFFSET),
        })
    }
}

/// `GET /v1/escrow/unclaimed?token=&platform=&chain=&limit=&offset=` — every
/// slot still holding something, bound handles and unbound alike: grouped by
/// token, the largest amount first within each.
pub(super) async fn unclaimed(
    State(state): State<AppState>,
    Query(params): Query<UnclaimedParams>,
) -> Result<Json<Unclaimed>, ApiError> {
    let page = params.page()?;
    state.synced(page.chain).await?;

    Ok(Json(Unclaimed {
        token: page.token,
        platform_id: page.platform_id,
        limit: page.limit,
        offset: page.offset,
        slots: EscrowAmount::all_of(state.store.unclaimed(page).await?)?,
    }))
}
