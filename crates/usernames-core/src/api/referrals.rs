//! `GET /v1/referrals/...`: the binds that named an address's handles as
//! their referrer, newest first and paged by a cursor.

use axum::{
    extract::State,
    Json,
};

use super::{
    model::{
        AddressReferrals,
        HandleRef,
        Referral,
    },
    ApiError,
    ApiPath,
    ApiQuery,
    AppState,
    PageParams,
    Param,
};
use crate::db::ReferralRow;

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
            block_time: row.block_time,
            referrer: HandleRef::of(
                Some(row.referrer_platform),
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
        .referrals(address, page)
        .await?
        .served(Referral::of)?;
    Ok(Json(AddressReferrals {
        address,
        referrals,
        next,
    }))
}
