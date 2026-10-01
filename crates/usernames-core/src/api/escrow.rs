//! `GET /v1/escrow/...`: what the escrow holds — for an address to claim or
//! refund, against one handle, and every slot still unclaimed.

use alloy::primitives::B256;
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
    handle_of,
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
    ApiError,
    AppState,
    ChainFilter,
};
use crate::{
    db::{
        EscrowSlotRow,
        UnclaimedFilter,
    },
    nodes,
};

/// An unclaimed page holds this many slots unless the request asks for fewer.
const DEFAULT_LIMIT: i64 = 20;
/// The most slots one page holds.
const MAX_LIMIT: i64 = 100;
/// How far into the list a page may start: deeper pages are a scan the
/// database repeats per request, and a client that far in wants a token or a
/// platform to narrow by.
const MAX_OFFSET: i64 = 10_000;

fn amount_of(row: EscrowSlotRow) -> Result<EscrowAmount, ApiError> {
    let unreadable = |what: &str| {
        ApiError::internal(format_args!(
            "escrow {what} {:?} on chain {}",
            row.amount, row.chain_id
        ))
    };
    let amount = row.amount().ok_or_else(|| unreadable("amount"))?;
    let round = row.round().ok_or_else(|| unreadable("round"))?;
    let platform_id = B256::from_slice(&row.platform_id);
    Ok(EscrowAmount {
        chain_id: row.chain_id,
        holder: row.holder_address(),
        token: alloy::primitives::Address::from_slice(&row.token),
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

fn amounts_of(rows: Vec<EscrowSlotRow>) -> Result<Vec<EscrowAmount>, ApiError> {
    rows.into_iter().map(amount_of).collect()
}

/// `GET /v1/escrow/address/{address}?chain=` — what is waiting for an address:
/// everything held for the handles it holds now, which `claim` pays it, and
/// its own deposits nobody has claimed yet, which `refund` pays back.
pub(super) async fn address(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<AddressBalances>, ApiError> {
    let address = parse_address(&address)?;
    let chain = filter.parse()?;
    state.synced(chain).await?;

    let claimable = amounts_of(state.store.claimable(chain, address).await?)?;
    let refundable = amounts_of(state.store.refundable(chain, address).await?)?;
    Ok(Json(AddressBalances {
        address,
        claimable,
        refundable,
    }))
}

/// `GET /v1/escrow/handle/{platform}/{handle}?chain=` — what a handle holds,
/// token by token, and who may claim it.
pub(super) async fn handle(
    State(state): State<AppState>,
    Path((platform, handle)): Path<(String, String)>,
    Query(filter): Query<ChainFilter>,
) -> Result<Json<HandleBalances>, ApiError> {
    let (platform, normalized) = handle_of(&platform, &handle)?;
    let chain = filter.parse()?;
    state.synced(chain).await?;

    let handle_node = nodes::handle_node(platform.id(), &normalized);
    let held = amounts_of(state.store.escrowed_for(chain, handle_node).await?)?;
    Ok(Json(HandleBalances {
        handle: HandleQuery {
            platform: platform.known(),
            platform_id: Some(platform.id()),
            handle_node,
            handle: Some(normalized.as_str().to_string()),
        },
        held,
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

    let held = amounts_of(state.store.escrowed_for(chain, handle_node).await?)?;
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

/// `GET /v1/escrow/unclaimed?token=&platform=&chain=&limit=&offset=` — every
/// slot still holding something, bound handles and unbound alike: grouped by
/// token, the largest amount first within each.
pub(super) async fn unclaimed(
    State(state): State<AppState>,
    Query(params): Query<UnclaimedParams>,
) -> Result<Json<Unclaimed>, ApiError> {
    let chain = ChainFilter {
        chain: params.chain,
    }
    .parse()?;
    state.synced(chain).await?;
    let token = params.token.as_deref().map(parse_address).transpose()?;
    let platform_id = params
        .platform
        .as_deref()
        .map(parse_platform)
        .transpose()?
        .map(|platform| platform.id());
    let limit = parse_count(params.limit.as_deref(), "limit")?
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let offset = parse_count(params.offset.as_deref(), "offset")?
        .unwrap_or(0)
        .clamp(0, MAX_OFFSET);

    let filter = UnclaimedFilter {
        chain,
        token,
        platform_id,
    };
    let slots = amounts_of(state.store.unclaimed(filter, limit, offset).await?)?;
    Ok(Json(Unclaimed {
        token,
        platform_id,
        limit,
        offset,
        slots,
    }))
}
