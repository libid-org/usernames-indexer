//! Every statement the store runs, one file each under `sql/`, named by
//! what it does. The Rust beside a use binds the parameters and says why;
//! the file says what. Read at compile time, so a missing file fails the
//! build rather than a request.

pub const CHAIN_STATE: &str = include_str!("../../sql/chain_state.sql");
pub const CHAIN_STATUSES: &str = include_str!("../../sql/chain_statuses.sql");
pub const CHAINS: &str = include_str!("../../sql/chains.sql");
pub const LOCK_CHAIN: &str = include_str!("../../sql/lock_chain.sql");
pub const CLAIM_CHAIN: &str = include_str!("../../sql/claim_chain.sql");
pub const RESET_CHAIN: &str = include_str!("../../sql/reset_chain.sql");
pub const HOLD_CHAIN: &str = include_str!("../../sql/hold_chain.sql");
pub const SET_CURSOR: &str = include_str!("../../sql/set_cursor.sql");
pub const SET_CHAIN_HEAD: &str = include_str!("../../sql/set_chain_head.sql");
pub const SET_DEPLOY_BLOCK: &str = include_str!("../../sql/set_deploy_block.sql");
pub const SET_PROOF_VERIFIER: &str = include_str!("../../sql/set_proof_verifier.sql");
pub const SET_WINDOW_ERROR: &str = include_str!("../../sql/set_window_error.sql");
#[cfg(feature = "test-seams")]
pub const SET_VALID_UNTIL: &str = include_str!("../../sql/set_valid_until.sql");
#[cfg(feature = "test-seams")]
pub const FORGET_CURSOR: &str = include_str!("../../sql/forget_cursor.sql");
pub const FORGET_CHAIN: &str = include_str!("../../sql/forget_chain.sql");
/// Every row an indexer wrote for one chain into the read model: what a
/// replay clears.
pub const CLEAR_CHAIN: &str = include_str!("../../sql/clear_chain.sql");
pub const WRITER_TRY_LOCK: &str = include_str!("../../sql/writer_try_lock.sql");
pub const WRITER_LOCK: &str = include_str!("../../sql/writer_lock.sql");
pub const WRITER_UNLOCK: &str = include_str!("../../sql/writer_unlock.sql");
pub const WRITER_ALIVE: &str = include_str!("../../sql/writer_alive.sql");
pub const SET_CHAIN_TARGET: &str = include_str!("../../sql/set_chain_target.sql");
pub const TOUCH_CHAIN_TARGET: &str = include_str!("../../sql/touch_chain_target.sql");
pub const JOURNAL_EVENT: &str = include_str!("../../sql/journal_event.sql");
pub const UPSERT_ID: &str = include_str!("../../sql/upsert_id.sql");
pub const UPSERT_HANDLE: &str = include_str!("../../sql/upsert_handle.sql");
pub const PUBLISH: &str = include_str!("../../sql/publish.sql");
pub const WITHDRAW_NAME: &str = include_str!("../../sql/withdraw_name.sql");
pub const PUBLISHED_NAME: &str = include_str!("../../sql/published_name.sql");
pub const RETIRE_HANDLE: &str = include_str!("../../sql/retire_handle.sql");
pub const ENS_ANSWER: &str = include_str!("../../sql/ens_answer.sql");
pub const CHAIN_NAMES_TAKEN: &str = include_str!("../../sql/chain_names_taken.sql");
/// The names the chain goes by in an ENS name.
pub const CLEAR_CHAIN_NAMES: &str = include_str!("../../sql/clear_chain_names.sql");
pub const INSERT_CHAIN_NAME: &str = include_str!("../../sql/insert_chain_name.sql");
pub const CHAIN_NAMED: &str = include_str!("../../sql/chain_named.sql");
pub const SYNCED: &str = include_str!("../../sql/synced.sql");
/// The projection every `names.ids` read shares: the row itself, the
/// handle it points at, and whether that pair is displayed. A caller
/// appends its own `WHERE`, the only part that differs.
pub const IDENTITY_PROJECTION: &str = include_str!("../../sql/identity_projection.sql");
/// The projection every `names.handles` read shares: the row and its
/// disclosed text. A caller appends its `WHERE`.
pub const HANDLE_PROJECTION: &str = include_str!("../../sql/handle_projection.sql");
/// Live, disclosed handles with their id node and display flag; the caller
/// narrows, orders and pages.
pub const SEARCH_PROJECTION: &str = include_str!("../../sql/search_projection.sql");
pub const ESCROW_DEPOSIT: &str = include_str!("../../sql/escrow_deposit.sql");
pub const ESCROW_BOOK_REFUNDABLE: &str =
    include_str!("../../sql/escrow_book_refundable.sql");
pub const ESCROW_CLAIM: &str = include_str!("../../sql/escrow_claim.sql");
pub const ESCROW_CLOSE_ROUND: &str = include_str!("../../sql/escrow_close_round.sql");
pub const ESCROW_REFUND: &str = include_str!("../../sql/escrow_refund.sql");
pub const ESCROW_TAKE_REFUNDABLE: &str =
    include_str!("../../sql/escrow_take_refundable.sql");
pub const PREVIOUS_HOLDERS: &str = include_str!("../../sql/previous_holders.sql");
pub const CLAIMED_SENDERS: &str = include_str!("../../sql/claimed_senders.sql");
pub const RECORD_ADDRESS_EVENT: &str = include_str!("../../sql/record_address_event.sql");
pub const RECORD_HANDLE_NODE: &str = include_str!("../../sql/record_handle_node.sql");
/// An address's history: the events it took part in, each with its
/// roles, the journal row and the handle the event concerns. The caller
/// names the address, narrows and pages.
pub const ADDRESS_HISTORY_PROJECTION: &str =
    include_str!("../../sql/address_history_projection.sql");
/// A handle's history, in the same shape without roles.
pub const HANDLE_HISTORY_PROJECTION: &str =
    include_str!("../../sql/handle_history_projection.sql");
/// Slots still holding something, with the handle and its holder; the
/// caller narrows, orders and pages.
pub const ESCROW_SLOT_PROJECTION: &str =
    include_str!("../../sql/escrow_slot_projection.sql");
/// The slots a holder may claim, read per handle; the caller narrows
/// and caps them.
pub const CLAIMABLE_SLOTS: &str = include_str!("../../sql/claimable_slots.sql");
/// The holder whose handles a claimable read walks; the caller narrows
/// the chain and the cursor.
pub const CLAIMABLE_HOLDER: &str = include_str!("../../sql/claimable_holder.sql");
/// One sender's contributions in the same shape, with their refund dates.
pub const REFUNDABLE_PROJECTION: &str =
    include_str!("../../sql/refundable_projection.sql");
