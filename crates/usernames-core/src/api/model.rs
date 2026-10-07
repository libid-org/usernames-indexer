//! The read API's answers, as the wire carries them.
//!
//! Serialized by the handlers and deserialized by whatever reads the API in
//! Rust — the integration suites first of all, so a test asserts on the type
//! a caller gets rather than on JSON it picks apart by hand. Every field
//! serializes in camelCase; addresses are EIP-55 strings and nodes 0x-hex.

use alloy::primitives::{
    Address,
    Bytes,
    B256,
    U256,
};
use serde::{
    Deserialize,
    Serialize,
};
use serde_with::{
    serde_as,
    DisplayFromStr,
};

pub use crate::events::Role;
use crate::{
    events::{
        EscrowEvent,
        NamesEvent,
    },
    nodes::KnownPlatform,
};

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
    /// The names the chain goes by in an ENS name, as its indexer declared
    /// them.
    pub names: Vec<String>,
    /// The contract the rows were indexed from, as the indexer recorded it.
    pub contract: Option<Address>,
    /// The HandleEscrow the escrow rows were indexed from, when the indexer
    /// watches one.
    pub escrow: Option<Address>,
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
    /// The account id node the handle points back at (`idNodeByHandle`).
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
    /// still the account's — the `handleNodeById`/`idNodeByHandle` round
    /// trip.
    pub handle: Option<String>,
    /// The handle node this account last proved (`handleNodeById`).
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

/// The handle an event or an amount concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleRef {
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The node the chain filed the handle under.
    pub handle_node: B256,
    /// The normalized handle, once a bind has carried its text. A deposit
    /// carries only the node, so a handle nobody has bound has none here.
    pub handle: Option<String>,
}

/// One event, typed by its `kind`: the journal kinds, with each event's
/// fields named the way the contract names them. A `uint256` is a decimal
/// string, as in the journal: token amounts do not fit a JSON number.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[allow(missing_docs)]
pub enum HistoryEvent {
    IdentityBound {
        holder: Address,
        id_node: B256,
        handle_node: B256,
        platform_id: B256,
        id: String,
        handle: String,
        observed_at: u64,
        published: bool,
        ceremony_version: u16,
    },
    HandleRetired {
        platform_id: B256,
        handle_node: B256,
        holder: Address,
    },
    HandleUnpublished {
        holder: Address,
        platform_id: B256,
    },
    PlatformConfigured {
        platform_id: B256,
    },
    ProofVerifierConfigured {
        verifier: Address,
    },
    CeremonyBound {
        authorization_digest: B256,
        holder: Address,
        platform_id: B256,
        client_identifier: Bytes,
    },
    BindFeePaid {
        authorization_digest: B256,
        receiver: Address,
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
    },
    Deposited {
        handle_node: B256,
        token: Address,
        refund_to: Address,
        depositor: Address,
        platform_id: B256,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
    },
    Forwarded {
        handle_node: B256,
        token: Address,
        depositor: Address,
        holder: Address,
        platform_id: B256,
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
        #[serde_as(as = "DisplayFromStr")]
        received: U256,
    },
    Claimed {
        handle_node: B256,
        token: Address,
        claimer: Address,
        recipient: Address,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        released: U256,
        #[serde_as(as = "DisplayFromStr")]
        received: U256,
    },
    Refunded {
        handle_node: B256,
        token: Address,
        refund_to: Address,
        recipient: Address,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        released: U256,
        #[serde_as(as = "DisplayFromStr")]
        received: U256,
    },
}

/// The wire's copy of a journaled event: the same fields, so a client reads
/// what the chain emitted, in a type the journal's format does not bind.
impl From<NamesEvent> for HistoryEvent {
    fn from(event: NamesEvent) -> Self {
        match event {
            NamesEvent::IdentityBound {
                holder,
                id_node,
                handle_node,
                platform_id,
                id,
                handle,
                observed_at,
                published,
                ceremony_version,
            } => Self::IdentityBound {
                holder,
                id_node,
                handle_node,
                platform_id,
                id,
                handle,
                observed_at,
                published,
                ceremony_version,
            },
            NamesEvent::HandleRetired {
                platform_id,
                handle_node,
                holder,
            } => Self::HandleRetired {
                platform_id,
                handle_node,
                holder,
            },
            NamesEvent::HandleUnpublished {
                holder,
                platform_id,
            } => Self::HandleUnpublished {
                holder,
                platform_id,
            },
            NamesEvent::PlatformConfigured { platform_id } => {
                Self::PlatformConfigured { platform_id }
            }
            NamesEvent::ProofVerifierConfigured { verifier } => {
                Self::ProofVerifierConfigured { verifier }
            }
            NamesEvent::CeremonyBound {
                authorization_digest,
                holder,
                platform_id,
                client_identifier,
            } => Self::CeremonyBound {
                authorization_digest,
                holder,
                platform_id,
                client_identifier,
            },
            NamesEvent::BindFeePaid {
                authorization_digest,
                receiver,
                amount,
            } => Self::BindFeePaid {
                authorization_digest,
                receiver,
                amount,
            },
            NamesEvent::Escrow(EscrowEvent::Deposited {
                handle_node,
                token,
                refund_to,
                depositor,
                platform_id,
                round,
                amount,
            }) => Self::Deposited {
                handle_node,
                token,
                refund_to,
                depositor,
                platform_id,
                round,
                amount,
            },
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                handle_node,
                token,
                depositor,
                holder,
                platform_id,
                amount,
                received,
            }) => Self::Forwarded {
                handle_node,
                token,
                depositor,
                holder,
                platform_id,
                amount,
                received,
            },
            NamesEvent::Escrow(EscrowEvent::Claimed {
                handle_node,
                token,
                claimer,
                recipient,
                round,
                released,
                received,
            }) => Self::Claimed {
                handle_node,
                token,
                claimer,
                recipient,
                round,
                released,
                received,
            },
            NamesEvent::Escrow(EscrowEvent::Refunded {
                handle_node,
                token,
                refund_to,
                recipient,
                round,
                released,
                received,
            }) => Self::Refunded {
                handle_node,
                token,
                refund_to,
                recipient,
                round,
                released,
                received,
            },
        }
    }
}

/// One entry in a history: an event, where and when it happened, and the
/// handle it concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// The chain it happened on.
    pub chain_id: i64,
    /// Its block.
    pub block_number: i64,
    /// Its index in the block.
    pub log_index: i64,
    /// The transaction that emitted it.
    pub tx_hash: B256,
    /// Its block's time, in unix seconds.
    pub block_time: i64,
    /// In an address history, every part the address plays in the event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
    /// The handle the event concerns, when it concerns one.
    pub handle: Option<HandleRef>,
    /// The event itself.
    pub event: HistoryEvent,
}

/// `GET /v1/history/address/{address}`: what an address took part in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressHistory {
    /// The address asked about.
    pub address: Address,
    /// Newest first: by block time, then chain, block and log index.
    pub entries: Vec<HistoryEntry>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

/// `GET /v1/history/handle/{platform}/{handle}` and
/// `GET /v1/history/node/{node}`: everything that happened to one handle,
/// from deposits made while nobody held it through every bind and claim
/// after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleHistory {
    /// The handle, as far as the store knows it: always its node.
    pub handle: HandleQuery,
    /// Newest first: by block time, then chain, block and log index.
    pub entries: Vec<HistoryEntry>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

/// A handle as a request named it: by its text, which names the platform
/// too, or by its node, which names neither until an event does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleQuery {
    /// The platform's short key, when known.
    pub platform: Option<KnownPlatform>,
    /// The platform id, when the request or an event named it.
    pub platform_id: Option<B256>,
    /// The node the chain files the handle under.
    pub handle_node: B256,
    /// The normalized handle, when the request or a bind carried its text.
    pub handle: Option<String>,
}

/// One amount waiting in the escrow, and what it waits for.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EscrowAmount {
    /// The chain the escrow holds it on.
    pub chain_id: i64,
    /// The handle it is held against.
    pub handle: HandleRef,
    /// Who holds the handle now and may claim it; absent while nobody does.
    pub holder: Option<Address>,
    /// The token; EIP-7528 `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE` is
    /// the chain's own coin. Anybody can deposit a token they wrote: show
    /// the ones you recognize.
    pub token: Address,
    /// Token units, as a decimal string.
    #[serde_as(as = "DisplayFromStr")]
    pub amount: U256,
    /// The slot's current round: how many claims it has seen.
    #[serde_as(as = "DisplayFromStr")]
    pub round: U256,
}

/// `GET /v1/escrow/address/{address}`: what is waiting for an address, and
/// what it is owed back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressBalances {
    /// The address asked about.
    pub address: Address,
    /// Held for handles the address holds now: `claim` pays it.
    pub claimable: Vec<EscrowAmount>,
    /// What the address may take back from deposits nobody has claimed yet,
    /// whether or not the handle has a holder: `refund` pays it.
    pub refundable: Vec<EscrowAmount>,
}

/// `GET /v1/escrow/handle/{platform}/{handle}` and `GET /v1/escrow/node/{node}`:
/// what one handle holds, token by token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleBalances {
    /// The handle, as far as the store knows it.
    pub handle: HandleQuery,
    /// One per chain and token still holding something.
    pub held: Vec<EscrowAmount>,
}

/// `GET /v1/escrow/unclaimed`: every slot still holding something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Unclaimed {
    /// The token the list was narrowed to, when one was asked for.
    pub token: Option<Address>,
    /// The platform the list was narrowed to, when one was asked for.
    pub platform_id: Option<B256>,
    /// The page size served.
    pub limit: i64,
    /// Token by token, descending, the largest amount first within each.
    pub slots: Vec<EscrowAmount>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}
