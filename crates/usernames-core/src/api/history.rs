//! `GET /v1/history/...`: what an address took part in, and what happened to
//! a handle, newest first and paged by a cursor.

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
    handle_of,
    model::{
        AddressHistory,
        HandleHistory,
        HandleQuery,
        HandleRef,
        HistoryEntry,
        HistoryEvent,
        Role,
    },
    parse_address,
    parse_count,
    parse_node,
    ApiError,
    AppState,
    ChainFilter,
};
use crate::{
    db::{
        HistoryCursor,
        HistoryRow,
    },
    nodes,
};

/// A page holds this many entries unless the request asks for fewer.
const DEFAULT_LIMIT: i64 = 20;
/// The most entries one page holds.
const MAX_LIMIT: i64 = 100;

/// `?chain=8453&before=<next>&limit=20`.
#[derive(Deserialize)]
pub(super) struct HistoryParams {
    chain: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

/// One page, parsed: the chain, where the page starts, and its size.
struct Page {
    chain: Option<i64>,
    before: Option<HistoryCursor>,
    limit: i64,
}

impl HistoryParams {
    fn parse(self) -> Result<Page, ApiError> {
        let chain = ChainFilter { chain: self.chain }.parse()?;
        let before = self
            .before
            .as_deref()
            .map(|raw| {
                HistoryCursor::parse(raw).ok_or_else(|| {
                    ApiError::bad_request(
                        "invalid_cursor",
                        format!("{raw:?} is not a cursor a history page handed out"),
                    )
                })
            })
            .transpose()?;
        let limit = parse_count(self.limit.as_deref(), "limit")?
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);
        Ok(Page {
            chain,
            before,
            limit,
        })
    }
}

impl Page {
    /// The entries a page serves out of the `limit + 1` rows it read, and the
    /// cursor for the next page — only when that extra row proved one exists.
    fn serve(
        &self,
        mut rows: Vec<HistoryRow>,
    ) -> Result<(Vec<HistoryEntry>, Option<String>), ApiError> {
        let more = rows.len() as i64 > self.limit;
        rows.truncate(self.limit as usize);
        let next = more
            .then(|| rows.last().map(|row| HistoryCursor::after(row).to_string()))
            .flatten();
        let entries = rows.into_iter().map(entry_of).collect::<Result<_, _>>()?;
        Ok((entries, next))
    }
}

fn entry_of(row: HistoryRow) -> Result<HistoryEntry, ApiError> {
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
        .map(|name| {
            Role::parse(name)
                .ok_or_else(|| ApiError::internal(format_args!("role {name:?}")))
        })
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
    Ok(HistoryEntry {
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

/// `GET /v1/history/address/{address}?chain=&before=&limit=` — every event the
/// address took part in, newest first, each with the parts it played: binds
/// and what they took from it, its deposits and refunds, what it claimed, was
/// paid, or was paid through a handle it holds.
pub(super) async fn address(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<AddressHistory>, ApiError> {
    let address: Address = parse_address(&address)?;
    let page = params.parse()?;
    state.synced(page.chain).await?;

    let rows = state
        .store
        .address_history(page.chain, address, page.before, page.limit + 1)
        .await?;
    let (entries, next) = page.serve(rows)?;
    Ok(Json(AddressHistory {
        address,
        entries,
        next,
    }))
}

/// `GET /v1/history/handle/{platform}/{handle}?chain=&before=&limit=` — every
/// event on the handle's node, newest first: deposits while nobody held it,
/// the binds that gave it a holder, and the claims and payments after.
pub(super) async fn handle(
    State(state): State<AppState>,
    Path((platform, handle)): Path<(String, String)>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HandleHistory>, ApiError> {
    let (platform, normalized) = handle_of(&platform, &handle)?;
    let page = params.parse()?;
    state.synced(page.chain).await?;

    let handle_node = nodes::handle_node(platform.id(), &normalized);
    let rows = state
        .store
        .handle_history(page.chain, handle_node, page.before, page.limit + 1)
        .await?;
    let (entries, next) = page.serve(rows)?;
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
pub(super) async fn node(
    State(state): State<AppState>,
    Path(node): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HandleHistory>, ApiError> {
    let handle_node = parse_node(&node)?;
    let page = params.parse()?;
    state.synced(page.chain).await?;

    let rows = state
        .store
        .handle_history(page.chain, handle_node, page.before, page.limit + 1)
        .await?;
    let (entries, next) = page.serve(rows)?;
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
