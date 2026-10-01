//! `GET /v1/history/...`: what an address took part in, and what happened to
//! a handle, newest first and paged by a cursor.

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
    model::{
        AddressHistory,
        HandleHistory,
        HandleQuery,
        HandleRef,
        HistoryEntry,
        HistoryEvent,
    },
    parse_address,
    parse_count,
    parse_node,
    parse_platform,
    reject_nul,
    ApiError,
    AppState,
    ChainFilter,
    PAGE_DEFAULT_LIMIT,
    PAGE_MAX_LIMIT,
};
use crate::{
    db::{
        HistoryCursor,
        HistoryPage,
        HistoryRow,
        HistoryRows,
    },
    nodes,
};

/// `?chain=8453&before=<next>&limit=20`.
#[derive(Deserialize)]
pub(crate) struct HistoryParams {
    chain: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

impl HistoryParams {
    fn page(self) -> Result<HistoryPage, ApiError> {
        let chain = ChainFilter { chain: self.chain }.parse()?;
        let before = self
            .before
            .as_deref()
            .map(|raw| {
                raw.parse::<HistoryCursor>().map_err(|_| {
                    ApiError::bad_request(
                        "invalid_cursor",
                        format!("{raw:?} is not a cursor a history page handed out"),
                    )
                })
            })
            .transpose()?;
        let limit = parse_count(self.limit.as_deref(), "limit")?
            .unwrap_or(PAGE_DEFAULT_LIMIT)
            .clamp(1, PAGE_MAX_LIMIT);
        Ok(HistoryPage {
            chain,
            before,
            limit,
        })
    }
}

impl HistoryEntry {
    /// The entry a stored row stands for. The journal is this build's own
    /// writing, so a row it cannot read back is an internal error.
    fn of(row: HistoryRow) -> Result<Self, ApiError> {
        let event = HistoryEvent::from_journal(&row.kind, row.payload).map_err(|e| {
            ApiError::internal(format_args!(
                "journal row {}/{}/{} ({}): {e}",
                row.chain_id, row.block_number, row.log_index, row.kind
            ))
        })?;
        let roles = row
            .roles
            .unwrap_or_default()
            .iter()
            .map(|name| name.parse().map_err(ApiError::internal))
            .collect::<Result<_, _>>()?;
        let handle = match (row.handle_node, row.platform_id) {
            (Some(node), Some(platform)) => {
                let platform_id = B256::from_slice(&platform);
                Some(HandleRef {
                    platform: nodes::Platform::known_of(platform_id),
                    platform_id,
                    handle_node: B256::from_slice(&node),
                    handle: row.handle,
                })
            }
            _ => None,
        };
        Ok(Self {
            chain_id: row.chain_id,
            block_number: row.block_number,
            log_index: row.log_index,
            tx_hash: B256::from_slice(&row.tx_hash),
            block_time: row.block_time,
            roles,
            handle,
            event,
        })
    }
}

impl HistoryEntry {
    /// A page's entries, and the cursor that fetches the next one.
    fn page(rows: HistoryRows) -> Result<(Vec<Self>, Option<String>), ApiError> {
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
    Path(address): Path<String>,
    Query(params): Query<HistoryParams>,
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
    Path((platform, handle)): Path<(String, String)>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HandleHistory>, ApiError> {
    let platform = parse_platform(&platform)?;
    reject_nul(&handle, "handle")?;
    let page = params.page()?;
    state.synced(page.chain).await?;
    let normalized = platform.normalize_query(&handle)?;

    let handle_node = nodes::handle_node(platform.id(), &normalized);
    let (entries, next) =
        HistoryEntry::page(state.store.handle_history(handle_node, page).await?)?;
    Ok(Json(HandleHistory {
        handle: HandleQuery {
            platform: platform.known(),
            platform_id: Some(platform.id()),
            handle_node,
            handle: Some(normalized.as_str().to_string()),
        },
        entries,
        next,
    }))
}

/// `GET /v1/history/node/{node}?chain=&before=&limit=` — the same, for a
/// handle known only by its node: one somebody deposited for and nobody has
/// bound, whose text no event has carried.
pub(crate) async fn node(
    State(state): State<AppState>,
    Path(node): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HandleHistory>, ApiError> {
    let handle_node = parse_node(&node)?;
    let page = params.page()?;
    state.synced(page.chain).await?;

    let (entries, next) =
        HistoryEntry::page(state.store.handle_history(handle_node, page).await?)?;
    // Every entry concerns this node, so any one names its platform, and its
    // text once a bind has carried it.
    let known = entries.iter().find_map(|entry| entry.handle.as_ref());
    Ok(Json(HandleHistory {
        handle: HandleQuery {
            platform: known.and_then(|handle| handle.platform),
            platform_id: known.map(|handle| handle.platform_id),
            handle_node,
            handle: known.and_then(|handle| handle.handle.clone()),
        },
        entries,
        next,
    }))
}
