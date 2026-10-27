//! `GET /v1/escrow/...`: what the escrow holds — for an address to claim or
//! refund, against one handle, and every slot still unclaimed — paged by a
//! cursor, since anybody can deposit a token they wrote.

use axum::{
    extract::State,
    Json,
};
use serde::Deserialize;

use super::{
    model::{
        AddressAmounts,
        AddressContributions,
        Contribution,
        EscrowAmount,
        HandleAmounts,
        HandleRef,
        Unclaimed,
    },
    ApiError,
    ApiPath,
    ApiQuery,
    AppState,
    HandleKey,
    PageParams,
    Param,
};
use crate::db::{
    ContributionRow,
    EscrowSlotRow,
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
}

impl Contribution {
    /// A stored contribution, under the rule of [`EscrowAmount::of`].
    fn of(row: ContributionRow) -> Result<Self, ApiError> {
        Ok(Self {
            amount: EscrowAmount::of(row.slot)?,
            refundable_at: row.refundable_at,
        })
    }
}

/// `GET /v1/escrow/claimable/{address}?chain=&before=&limit=` — everything
/// held for the handles the address holds now, which `claim` pays it.
pub(crate) async fn claimable(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<AddressAmounts>, ApiError> {
    let address = address.parse_address()?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (amounts, next) = state
        .store
        .claimable(address, page)
        .await?
        .served(EscrowAmount::of)?;
    Ok(Json(AddressAmounts {
        address,
        amounts,
        next,
    }))
}

/// `GET /v1/escrow/refundable/{address}?chain=&before=&limit=` — what the
/// address sent that nobody has claimed yet, which `refund` pays back from
/// each contribution's `refundableAt`.
pub(crate) async fn refundable(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<AddressContributions>, ApiError> {
    let address = address.parse_address()?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (amounts, next) = state
        .store
        .refundable(address, page)
        .await?
        .served(Contribution::of)?;
    Ok(Json(AddressContributions {
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
    state
        .escrow_of(HandleKey::of_text(&platform, &handle)?, params)
        .await
}

/// `GET /v1/escrow/node/{node}?chain=&before=&limit=` — the same, for a
/// handle known only by its node.
pub(crate) async fn node(
    State(state): State<AppState>,
    ApiPath(node): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleAmounts>, ApiError> {
    state.escrow_of(HandleKey::of_node(&node)?, params).await
}

impl AppState {
    async fn escrow_of(
        &self,
        key: HandleKey,
        params: PageParams,
    ) -> Result<Json<HandleAmounts>, ApiError> {
        let page = params.page()?;
        self.synced(page.chain).await?;

        let (amounts, next) = self
            .store
            .escrowed_for(key.node(), page)
            .await?
            .served(EscrowAmount::of)?;
        let known = amounts.first().map(|amount| &amount.handle);
        Ok(Json(HandleAmounts {
            handle: HandleRef::asked(&key, known),
            amounts,
            next,
        }))
    }
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
    let token = params
        .token
        .as_deref()
        .map(str::parse_address)
        .transpose()?;
    let page = params.page.page()?;
    state.synced(page.chain).await?;

    let (amounts, next) = state
        .store
        .unclaimed(token, page)
        .await?
        .served(EscrowAmount::of)?;
    Ok(Json(Unclaimed {
        token,
        limit: page.limit,
        amounts,
        next,
    }))
}
