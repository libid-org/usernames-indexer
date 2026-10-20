//! `GET /v1/escrow/...`: what the escrow holds — for an address to claim or
//! refund, against one handle, and every slot still unclaimed — paged by a
//! cursor, since anybody can deposit a token they wrote.

use std::fmt::Display;

use axum::{
    extract::State,
    Json,
};
use serde::Deserialize;

use super::{
    model::{
        AddressAmounts,
        EscrowAmount,
        HandleAmounts,
        HandleQuery,
        HandleRef,
        Unclaimed,
    },
    parse_address,
    ApiError,
    ApiPath,
    ApiQuery,
    AppState,
    HandleKey,
    PageParams,
};
use crate::db::{
    EscrowSlotRow,
    Rows,
};

impl EscrowAmount {
    /// The amount a stored slot holds. The books are this build's own
    /// writing, so a value that is not a `uint256` is an internal error.
    fn of(row: EscrowSlotRow) -> Result<Self, ApiError> {
        let (Some(amount), Some(round)) = (row.amount(), row.round()) else {
            return Err(ApiError::internal(format_args!(
                "escrow slot {}/{}/{}: amount {:?} round {:?}",
                row.chain_id, row.handle_node, row.token, row.amount, row.round
            )));
        };
        Ok(Self {
            chain_id: row.chain_id,
            holder: row.holder,
            token: row.token,
            amount,
            round,
            handle: HandleRef::of(row.platform_id, row.handle_node, row.handle),
        })
    }

    /// A page's amounts, and the cursor that fetches the next one.
    fn page<C: Display>(
        rows: Rows<EscrowSlotRow, C>,
    ) -> Result<(Vec<Self>, Option<String>), ApiError> {
        let amounts = rows
            .rows
            .into_iter()
            .map(Self::of)
            .collect::<Result<_, _>>()?;
        Ok((amounts, rows.next.map(|next| next.to_string())))
    }
}

/// `GET /v1/escrow/claimable/{address}?chain=&before=&limit=` — everything
/// held for the handles the address holds now, which `claim` pays it.
pub(crate) async fn claimable(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<AddressAmounts>, ApiError> {
    let address = parse_address(&address)?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (amounts, next) =
        EscrowAmount::page(state.store.claimable(address, page).await?)?;
    Ok(Json(AddressAmounts {
        address,
        amounts,
        next,
    }))
}

/// `GET /v1/escrow/refundable/{address}?chain=&before=&limit=` — what deposits
/// naming the address as `refundTo` booked and nobody has claimed yet, which
/// `refund` pays back.
pub(crate) async fn refundable(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<AddressAmounts>, ApiError> {
    let address = parse_address(&address)?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (amounts, next) =
        EscrowAmount::page(state.store.refundable(address, page).await?)?;
    Ok(Json(AddressAmounts {
        address,
        amounts,
        next,
    }))
}

/// `GET /v1/escrow/handle/{platform}/{handle}?chain=&before=&limit=` — what a
/// handle holds, token by token, and who may claim it.
pub(crate) async fn handle(
    State(state): State<AppState>,
    ApiPath((platform, handle)): ApiPath<(String, String)>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleAmounts>, ApiError> {
    of_handle(&state, HandleKey::of_text(&platform, &handle)?, params).await
}

/// `GET /v1/escrow/node/{node}?chain=&before=&limit=` — the same, for a
/// handle known only by its node.
pub(crate) async fn node(
    State(state): State<AppState>,
    ApiPath(node): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleAmounts>, ApiError> {
    of_handle(&state, HandleKey::of_node(&node)?, params).await
}

async fn of_handle(
    state: &AppState,
    key: HandleKey,
    params: PageParams,
) -> Result<Json<HandleAmounts>, ApiError> {
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (amounts, next) =
        EscrowAmount::page(state.store.escrowed_for(key.node(), page).await?)?;
    let known = amounts.first().map(|amount| &amount.handle);
    Ok(Json(HandleAmounts {
        handle: HandleQuery::of(&key, known),
        amounts,
        next,
    }))
}

/// `?token=0x…&chain=8453&before=<next>&limit=20`.
#[derive(Deserialize)]
pub(crate) struct UnclaimedParams {
    token: Option<String>,
    #[serde(flatten)]
    page: PageParams,
}

/// `GET /v1/escrow/unclaimed?token=&chain=&before=&limit=` — every slot still
/// holding something, bound handles and unbound alike: grouped by token, the
/// largest amount first within each. An amount that moves between two pages
/// moves in the order too, so that slot may show twice or not at all.
pub(crate) async fn unclaimed(
    State(state): State<AppState>,
    ApiQuery(params): ApiQuery<UnclaimedParams>,
) -> Result<Json<Unclaimed>, ApiError> {
    let token = params.token.as_deref().map(parse_address).transpose()?;
    let page = params.page.page()?;
    state.synced(page.chain).await?;

    let (amounts, next) = EscrowAmount::page(state.store.unclaimed(token, page).await?)?;
    Ok(Json(Unclaimed {
        token,
        limit: page.limit,
        amounts,
        next,
    }))
}
