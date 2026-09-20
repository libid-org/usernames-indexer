//! The read API's answers, as the wire carries them.
//!
//! Serialized by the handlers and deserialized by whatever reads the API in
//! Rust — the integration suites first of all, so a test asserts on the type
//! a caller gets rather than on JSON it picks apart by hand. Every field
//! serializes in camelCase; addresses are EIP-55 strings and nodes 0x-hex.

use alloy::primitives::{
    Address,
    B256,
};
use serde::{
    Deserialize,
    Serialize,
};

use crate::nodes::KnownPlatform;

/// One error shape for the whole API, beside the HTTP status: a stable code
/// and prose. The code is the machine-readable half of the contract — a UI
/// that renders "retired" differently from "never claimed" branches on it,
/// not on English sentences that may be reworded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// The error itself.
    pub error: ErrorDetail,
}

/// The two halves of an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// Stable and machine-readable.
    pub code: String,
    /// For humans; may be reworded.
    pub message: String,
}

/// One chain the store holds, as its indexer last left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainStatus {
    /// The chain.
    pub chain_id: i64,
    /// The contract the rows were indexed from, as the indexer recorded it.
    pub contract: Option<Address>,
    /// The cursor: the last block whose window committed.
    pub last_indexed_block: Option<u64>,
    /// The chain head as the indexer last saw it.
    pub chain_head_block: Option<u64>,
    /// Blocks between the head the loop last saw and the cursor. Small and
    /// steady is healthy; growing means the loop is stalled or starved —
    /// `last_window_error` says which.
    pub lag_blocks: Option<u64>,
    /// Unix seconds at which the indexer last reported. `lag_blocks` cannot
    /// show a stopped loop — its two terms are both that loop's writes and
    /// freeze together — so watch this and `report_valid_for` too.
    pub indexer_reported_at: Option<u64>,
    /// Seconds until the indexer's last report expires, negative once it
    /// has: past zero the ENS gateway refuses this chain's index.
    pub report_valid_for: Option<i64>,
    /// The last window failure, while the most recent window failed.
    pub last_window_error: Option<String>,
    /// The Proof Verifier the contract is wired to, from its
    /// `ProofVerifierConfigured`: the one contract that checks every claim
    /// on this chain. Absent until the indexer has seen one.
    pub proof_verifier: Option<Address>,
}

/// `GET /v1/status`: every chain the store holds. Empty on a database no
/// indexer has touched, and still a 200 — the status is how a caller learns
/// that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// Every chain, by id.
    pub chains: Vec<ChainStatus>,
    /// The read-model version the indexers write.
    pub indexer_version: String,
}

/// One chain's answer to `resolveHandle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleBinding {
    /// The chain the handle is bound on.
    pub chain_id: i64,
    /// The storage key the chain filed the handle under.
    pub handle_node: B256,
    /// The wallet the handle resolves to.
    pub owner: Address,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// The plaintext account id the handle points back at, when ever bound.
    pub user_id: Option<String>,
    /// The account id node the handle points back at (`idOfHandle`).
    pub id_node: B256,
}

/// `GET /v1/resolve/handle/{platform}/{handle}`: the handle and the wallet it
/// resolves to on each chain it is bound on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleResolution {
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The handle as the chain keyed it: normalized.
    pub handle: String,
    /// One per chain, in chain order; never empty.
    pub bindings: Vec<HandleBinding>,
}

/// One chain's answer to `resolveId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdBinding {
    /// The chain the account is bound on.
    pub chain_id: i64,
    /// The storage key the chain filed the account under.
    pub id_node: B256,
    /// The wallet that proved the account.
    pub owner: Address,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// The handle this account last proved, when the node it points at is
    /// still the account's — the `handleOfId`/`idOfHandle` round trip.
    pub handle: Option<String>,
    /// The handle node this account last proved (`handleOfId`).
    pub handle_node: B256,
    /// Whether the handle is the wallet's displayed name on the platform.
    pub published: bool,
}

/// `GET /v1/resolve/id/{platform}/{userId}`: an account id and the wallet it
/// resolves to on each chain it is bound on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdResolution {
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The account id, byte-verbatim as the chain keys it.
    pub user_id: String,
    /// One per chain, in chain order; never empty.
    pub bindings: Vec<IdBinding>,
}

/// One identity a wallet proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressIdentity {
    /// The chain the identity was proved on: a wallet is the same address
    /// everywhere, its bindings are not.
    pub chain_id: i64,
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The account id, byte-verbatim.
    pub user_id: String,
    /// The handle the account holds, while it is still the account's.
    pub handle: Option<String>,
    /// The handle node the account last proved.
    pub handle_node: B256,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// Whether the handle still resolves to this wallet.
    pub resolves: bool,
    /// Whether this is the wallet's displayed name on the platform.
    pub published: bool,
}

/// `GET /v1/resolve/address/{address}`: every identity a wallet proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressResolution {
    /// The wallet asked about.
    pub address: Address,
    /// In chain, platform, account-id order.
    pub identities: Vec<AddressIdentity>,
}

/// One search hit: a live handle, its owner, and the id it pairs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// The chain the handle is bound on.
    pub chain_id: i64,
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The normalized handle.
    pub handle: String,
    /// The wallet it resolves to.
    pub owner: Address,
    /// The account id behind it, when bound.
    pub user_id: Option<String>,
    /// Whether the owner displays this handle.
    pub published: bool,
}

/// `GET /v1/search`: the page asked for, and what it was matched against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    /// The folded text the hits were matched against, when there was one.
    pub query: Option<String>,
    /// The wallet the hits are linked to, when one was asked for.
    pub owner: Option<Address>,
    /// The page size served.
    pub limit: i64,
    /// The page start served; a page shorter than `limit` is the last one.
    pub offset: i64,
    /// Ranked: exact, prefix, substring, fuzzy; then handle and chain.
    pub hits: Vec<SearchHit>,
}
