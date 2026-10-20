//! `GET /v1/history/...`: what an address took part in, and what happened to
//! a handle, newest first and paged by a cursor.

use axum::{
    extract::State,
    Json,
};

use super::{
    model::{
        AddressHistory,
        HandleHistory,
        HandleQuery,
        HandleRef,
        HistoryEntry,
    },
    parse_address,
    ApiError,
    ApiPath,
    ApiQuery,
    AppState,
    HandleKey,
    PageParams,
};
use crate::{
    db::{
        HistoryCursor,
        HistoryRow,
        Rows,
    },
    events::NamesEvent,
};

impl HistoryEntry {
    /// The entry a stored row stands for. The journal is this build's own
    /// writing, so a row it cannot read back is an internal error.
    fn of(row: HistoryRow) -> Result<Self, ApiError> {
        let at = format!(
            "journal row {}/{}/{} ({})",
            row.chain_id, row.block_number, row.log_index, row.kind
        );
        let event = NamesEvent::from_journal(&row.kind, row.payload)
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
        let handle = match (row.handle_node, row.platform_id) {
            (Some(handle_node), Some(platform_id)) => {
                Some(HandleRef::of(platform_id, handle_node, row.handle))
            }
            _ => None,
        };
        Ok(Self {
            chain_id: row.chain_id,
            block_number: row.block_number,
            log_index: row.log_index,
            tx_hash: row.tx_hash,
            block_time: row.block_time,
            roles,
            handle,
            event: event.into(),
        })
    }

    /// A page's entries, and the cursor that fetches the next one.
    fn page(
        rows: Rows<HistoryRow, HistoryCursor>,
    ) -> Result<(Vec<Self>, Option<String>), ApiError> {
        let entries = rows
            .rows
            .into_iter()
            .map(Self::of)
            .collect::<Result<_, _>>()?;
        Ok((entries, rows.next.map(|next| next.to_string())))
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
    let address = parse_address(&address)?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (entries, next) =
        HistoryEntry::page(state.store.address_history(address, page).await?)?;
    Ok(Json(AddressHistory {
        address,
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
    of_handle(&state, HandleKey::of_text(&platform, &handle)?, params).await
}

/// `GET /v1/history/node/{node}?chain=&before=&limit=` — the same, for a
/// handle known only by its node.
pub(crate) async fn node(
    State(state): State<AppState>,
    ApiPath(node): ApiPath<String>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<HandleHistory>, ApiError> {
    of_handle(&state, HandleKey::of_node(&node)?, params).await
}

async fn of_handle(
    state: &AppState,
    key: HandleKey,
    params: PageParams,
) -> Result<Json<HandleHistory>, ApiError> {
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (entries, next) =
        HistoryEntry::page(state.store.handle_history(key.node(), page).await?)?;
    let known = entries.iter().find_map(|entry| entry.handle.as_ref());
    Ok(Json(HandleHistory {
        handle: HandleQuery::of(&key, known),
        entries,
        next,
    }))
}
