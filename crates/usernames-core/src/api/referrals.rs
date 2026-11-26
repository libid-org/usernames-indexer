//! `GET /v1/referrals/...`: the binds that named an address's handles, or
//! one handle, as their referrer, newest first and paged by a cursor.

use axum::{
    extract::State,
    Json,
};

use super::{
    model::{
        AddressReferrals,
        HandleRef,
        HandleReferrals,
        Referral,
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
    ReferralRow,
    Referrer,
};

impl Referral {
    /// The referral a stored row stands for. The rows are this build's own
    /// writing, so one it cannot read back is an internal error.
    fn of(row: ReferralRow) -> Result<Self, ApiError> {
        let at = format!(
            "referral {}/{}/{}",
            row.chain_id, row.block_number, row.log_index
        );
        let (Some(fee), Some(share)) = (row.fee(), row.share()) else {
            return Err(ApiError::internal(format_args!(
                "{at}: fee {:?} share {:?}",
                row.fee, row.share
            )));
        };
        let outcome = row
            .outcome
            .parse()
            .map_err(|e| ApiError::internal(format_args!("{at}: {e}")))?;
        let delivery = row
            .delivery
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|e| ApiError::internal(format_args!("{at}: {e}")))?;
        Ok(Self {
            chain_id: row.chain_id,
            block_number: row.block_number,
            log_index: row.log_index,
            tx_hash: row.tx_hash,
            block_time: row.block_time,
            referrer: HandleRef::of(
                row.referrer_platform,
                row.referrer_node,
                row.referrer_handle,
            ),
            binder: row.binder,
            binder_handle: row.binder_handle,
            fee,
            share,
            outcome,
            delivery,
        })
    }
}

/// `GET /v1/referrals/address/{address}?chain=&before=&limit=` — every bind
/// that named a handle the address holds as its referrer, on any chain,
/// newest first, with the share and what became of it.
pub(crate) async fn address(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<AddressReferrals>, ApiError> {
    let address = address.parse_address()?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (referrals, next) = state
        .store
        .referrals(Referrer::Holder(address), page)
        .await?
        .served(Referral::of)?;
    Ok(Json(AddressReferrals {
        address,
        referrals,
        next,
    }))
}

/// `GET /v1/referrals/handle/{platform}/{handle}?chain=&before=&limit=` —
/// every bind that named the handle as its referrer, on any chain, newest
/// first, whoever holds the handle and whether or not anybody does.
pub(crate) async fn handle(
    State(state): State<AppState>,
    ApiPath((platform, handle)): ApiPath<(String, String)>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleReferrals>, ApiError> {
    state
        .referrals_of(HandleKey::of_text(&platform, &handle)?, params)
        .await
}

/// `GET /v1/referrals/node/{node}?chain=&before=&limit=` — the same, for a
/// handle known only by its node.
pub(crate) async fn node(
    State(state): State<AppState>,
    ApiPath(node): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleReferrals>, ApiError> {
    state.referrals_of(HandleKey::of_node(&node)?, params).await
}

impl AppState {
    /// A handle's referrals, each naming the handle as the store knows it
    /// on any chain.
    async fn referrals_of(
        &self,
        key: HandleKey,
        params: PageParams,
    ) -> Result<Json<HandleReferrals>, ApiError> {
        let page = params.page()?;
        self.synced(page.chain).await?;

        let (rows, known) = tokio::try_join!(
            self.store.referrals(Referrer::Node(key.node()), page),
            self.store.handle_node(key.node(), None),
        )?;
        let handle = HandleRef::asked(&key, known);
        let (referrals, next) = rows.served(|row| {
            Ok(Referral {
                referrer: handle.clone(),
                ..Referral::of(row)?
            })
        })?;
        Ok(Json(HandleReferrals {
            handle,
            referrals,
            next,
        }))
    }
}
