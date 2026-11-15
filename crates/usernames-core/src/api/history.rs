//! `GET /v1/history/...`: what an address took part in, and what happened to
//! a handle, newest first and paged by a cursor.

use std::fmt::Display;

use alloy::primitives::B256;
use axum::{
    extract::State,
    Json,
};

use super::{
    model::{
        AddressHistory,
        ChainHistory,
        HandleHistory,
        HandleRef,
        HistoryEntry,
        ReferralLink,
    },
    ApiError,
    ApiPath,
    ApiQuery,
    AppState,
    ChainHistoryParams,
    HandleKey,
    PageParams,
    Param,
};
use crate::{
    db::{
        HistoryRow,
        ReferralLinkRow,
        Rows,
    },
    events::NamesEvent,
};

impl ReferralLink {
    /// The link a stored row stands for, its referrer on `platform_id`.
    fn of(row: ReferralLinkRow, platform_id: Option<B256>) -> Self {
        Self {
            referrer: HandleRef::of(platform_id, row.referrer_node, row.referrer_handle),
            binder: row.binder,
            binder_handle: row.binder_handle,
            log_index: row.referral_log_index,
        }
    }
}

impl HistoryEntry {
    /// The entry a stored row stands for, with the referral it links to.
    /// The journal is this build's own writing, so a row it cannot read back
    /// is an internal error.
    fn of(row: HistoryRow, link: Option<ReferralLinkRow>) -> Result<Self, ApiError> {
        let at = format!(
            "journal row {}/{}/{}",
            row.chain_id, row.block_number, row.log_index
        );
        let event = NamesEvent::try_from(&row.event)
            .map_err(|e| ApiError::internal(format_args!("{at}: {e}")))?;
        let roles = row
            .roles
            .unwrap_or_default()
            .iter()
            .map(|name| {
                name.parse()
                    .map_err(|e| ApiError::internal(format_args!("{at}: {e}")))
            })
            .collect::<Result<_, _>>()?;
        // A `referred` entry names its referrer's platform; a payment's
        // handle is the referrer's.
        let referral = link.map(|link| {
            ReferralLink::of(link, row.event.referrer_platform.or(row.node_platform_id))
        });
        let handle = row.event.handle_node.map(|handle_node| {
            HandleRef::of(row.node_platform_id, handle_node, row.handle)
        });
        Ok(Self {
            chain_id: row.chain_id,
            block_number: row.block_number,
            log_index: row.log_index,
            tx_hash: row.tx_hash,
            block_time: row.block_time,
            roles,
            handle,
            referral,
            event: event.into(),
        })
    }
}

/// `GET /v1/history/address/{address}?chain=&before=&limit=` — every event the
/// address took part in, newest first, each with the parts it played: binds
/// and what they took from it, its deposits and refunds, what it claimed, was
/// paid, or was paid through a handle it holds.
pub(crate) async fn address(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<AddressHistory>, ApiError> {
    let address = address.parse_address()?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let rows = state.store.address_history(address, page).await?;
    let (entries, next) = state.entries(rows).await?;
    Ok(Json(AddressHistory {
        address,
        entries,
        next,
    }))
}

/// `GET /v1/history/chain/{chain}?day=&kind=&token=&before=&limit=` — what
/// happened on one chain, newest first: on one UTC day or over its whole
/// journal, of one kind and in one token when asked. Each page is one index
/// seek and at most `limit` entries, however busy the day.
pub(crate) async fn chain(
    State(state): State<AppState>,
    ApiPath(chain): ApiPath<String>,
    ApiQuery(params): ApiQuery<ChainHistoryParams>,
) -> Result<Json<ChainHistory>, ApiError> {
    let (events, page) = params.on(&chain)?;
    state.synced(page.chain).await?;

    let read = state.store.chain_history(events, page).await?;
    let (entries, next) = state.entries(read.rows).await?;
    Ok(Json(ChainHistory {
        computed_at: read.taken_at,
        chain_id: events.chain_id,
        day: events.day,
        entries,
        next,
    }))
}

/// `GET /v1/history/handle/{platform}/{handle}?chain=&before=&limit=` — every
/// event on the handle's node, newest first: deposits while nobody held it,
/// the binds that gave it a holder, and the claims and payments after.
pub(crate) async fn handle(
    State(state): State<AppState>,
    ApiPath((platform, handle)): ApiPath<(String, String)>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleHistory>, ApiError> {
    state
        .history_of(HandleKey::of_text(&platform, &handle)?, params)
        .await
}

/// `GET /v1/history/node/{node}?chain=&before=&limit=` — the same, for a
/// handle known only by its node.
pub(crate) async fn node(
    State(state): State<AppState>,
    ApiPath(node): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleHistory>, ApiError> {
    state.history_of(HandleKey::of_node(&node)?, params).await
}

impl AppState {
    /// A history page's entries, each with the referral it links to, and
    /// the cursor of the next page.
    async fn entries<C: Display>(
        &self,
        rows: Rows<HistoryRow, C>,
    ) -> Result<(Vec<HistoryEntry>, Option<String>), ApiError> {
        let mut links = self.store.referral_links(&rows.rows).await?;
        rows.served(|row| {
            let link = links.take(&row);
            HistoryEntry::of(row, link)
        })
    }

    async fn history_of(
        &self,
        key: HandleKey,
        params: PageParams,
    ) -> Result<Json<HandleHistory>, ApiError> {
        let page = params.page()?;
        let chain = page.chain;
        self.synced(chain).await?;

        let (rows, known) = tokio::try_join!(
            self.store.handle_history(key.node(), page),
            self.store.handle_node(key.node(), chain),
        )?;
        let (entries, next) = self.entries(rows).await?;
        Ok(Json(HandleHistory {
            handle: HandleRef::asked(&key, known),
            entries,
            next,
        }))
    }
}
