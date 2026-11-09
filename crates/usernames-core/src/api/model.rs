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
    U512,
};
use serde::{
    Deserialize,
    Serialize,
};
use serde_with::{
    serde_as,
    DisplayFromStr,
};

pub use crate::events::{
    Delivery,
    Outcome,
    Role,
};
use crate::{
    events::{
        EscrowEvent,
        NamesEvent,
    },
    nodes::KnownPlatform,
    referral::Code,
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

/// Why a request was refused, stable and machine-readable: a UI branches on
/// it, never on the prose beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No chain in scope binds the handle.
    HandleNotBound,
    /// The handle was bound and retired: it no longer resolves.
    HandleRetired,
    /// No chain in scope binds the account.
    IdNotBound,
    /// No chain has bound a handle node the referral code names.
    CodeNotIssued,
    /// More than one bound handle node starts with the referral code's
    /// bytes, so it names none; each node goes by a longer code.
    CodeAmbiguous,
    /// Text the platform could never hold as a handle.
    HandleImpossible,
    /// No indexer has committed a first window for the chains in scope.
    NotSynced,
    /// Neither a known platform key nor a 0x-hex platform id.
    InvalidPlatform,
    /// Not an address.
    InvalidAddress,
    /// Not a 0x-hex 32-byte handle node.
    InvalidNode,
    /// Not a referral code: the base58 of 8 to 32 bytes.
    InvalidCode,
    /// Not a chain id.
    InvalidChain,
    /// Not a cursor the list handed out.
    InvalidCursor,
    /// A query value the route refuses.
    InvalidArgument,
    /// A path that does not parse.
    InvalidPath,
    /// A query string that does not parse.
    InvalidQuery,
    /// A failure of the service, whatever the request.
    Internal,
}

/// The two halves of an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// Stable and machine-readable.
    pub code: ErrorCode,
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
    /// The ReferralSplitter the referrals were indexed from, when the
    /// indexer watches one.
    pub referral_splitter: Option<Address>,
    /// The cursor: the last block whose window committed.
    pub last_indexed_block: Option<u64>,
    /// The chain head as the indexer last saw it.
    pub chain_head_block: Option<u64>,
    /// Blocks the cursor trails the target, the block the indexer last set
    /// out to reach, `CONFIRMATIONS` behind the head it saw; the number
    /// `/ens/status` reports and `ENS_MAX_LAG_BLOCKS` bounds. Small and
    /// steady is healthy; growing means the loop is stalled or starved —
    /// `last_window_error` says which. Absent before the first window.
    pub lag_blocks: Option<u64>,
    /// Unix seconds at which the indexer last reported. `lag_blocks` cannot
    /// show a stopped loop — its two terms are both that loop's writes and
    /// freeze together — so watch this and `report_valid_for` too.
    pub indexer_reported_at: Option<u64>,
    /// Seconds until the indexer's last report expires, negative once it
    /// has: past zero the ENS gateway refuses this chain's index. Absent
    /// before a report, or when the stored expiry is out of range, which the
    /// gateway refuses too.
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
    /// The wallet the handle resolves to.
    pub owner: Address,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// The account id node the handle points back at (`idNodeByHandle`).
    pub id_node: B256,
    /// The handle's text, once this chain disclosed it.
    pub handle: Option<String>,
}

/// `GET /v1/resolve/handle/{platform}/{handle}` and
/// `GET /v1/resolve/handle-node/{handleNode}`: the wallet a handle node
/// resolves to on each chain it is bound on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleResolution {
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The node the chain filed the handle under.
    pub handle_node: B256,
    /// One per chain, in chain order; never empty.
    pub bindings: Vec<HandleBinding>,
}

/// One chain's answer to `resolveId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdBinding {
    /// The chain the account is bound on.
    pub chain_id: i64,
    /// The wallet that proved the account.
    pub owner: Address,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// The handle's text, once disclosed and while still the account's — the
    /// `handleNodeById`/`idNodeByHandle` round trip.
    pub handle: Option<String>,
    /// The handle node this account last proved (`handleNodeById`).
    pub handle_node: B256,
    /// Whether the handle is the wallet's name.
    pub published: bool,
}

/// `GET /v1/resolve/id-node/{idNode}`: the wallet an id node resolves to on
/// each chain it is bound on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdResolution {
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The node the chain filed the account under.
    pub id_node: B256,
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
    /// The node the chain filed the account under.
    pub id_node: B256,
    /// The handle's text, once disclosed and while still the account's.
    pub handle: Option<String>,
    /// The handle node the account last proved.
    pub handle_node: B256,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// Whether the handle still resolves to this wallet.
    pub resolves: bool,
    /// Whether this is the wallet's name.
    pub published: bool,
}

/// `GET /v1/resolve/address/{address}`: every identity a wallet proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressResolution {
    /// The wallet asked about.
    pub address: Address,
    /// In chain, platform, id-node order.
    pub identities: Vec<AddressIdentity>,
}

/// One search hit: a live, disclosed handle, its owner, and the id node it
/// pairs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// The chain the handle is bound on.
    pub chain_id: i64,
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// The handle's text.
    pub handle: String,
    /// The node the chain filed the handle under.
    pub handle_node: B256,
    /// The wallet it resolves to.
    pub owner: Address,
    /// The account id node behind it.
    pub id_node: B256,
    /// Whether this handle is the owner's name.
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

/// The handle an event, an amount or a request concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleRef {
    /// The platform's short key, when known.
    pub platform: Option<KnownPlatform>,
    /// The platform id, once a request or an event named it (a bind, a
    /// reveal, a publish or a retire): a deposit names only the node.
    pub platform_id: Option<B256>,
    /// The node the chain filed the handle under.
    pub handle_node: B256,
    /// The handle's text, once the chain disclosed it.
    pub handle: Option<String>,
}

/// `GET /v1/referral/code/{code}` and `GET /v1/referral/node/{node}/code`:
/// the handle a referral code names, and how a landing page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Referrer {
    /// The code asked for, or the shortest code that names the node.
    pub code: Code,
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform the handle lives on.
    pub platform_id: B256,
    /// The node the code names.
    pub handle_node: B256,
    /// The handle's text, where a holder published it as its name; `None`
    /// keeps the handle hidden.
    pub primary_handle: Option<String>,
    /// The chains the handle is bound on now; empty once retired everywhere,
    /// when the code still names it.
    pub chains: Vec<i64>,
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
        observed_at: u64,
        ceremony_version: u16,
    },
    HandleRevealed {
        platform_id: B256,
        handle_node: B256,
    },
    HandlePublished {
        holder: Address,
        platform_id: B256,
        handle_node: B256,
    },
    HandleRetired {
        platform_id: B256,
        handle_node: B256,
        holder: Address,
    },
    HandleUnpublished {
        holder: Address,
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
        sender: Address,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        amount: U256,
        refundable_at: u64,
    },
    Forwarded {
        handle_node: B256,
        token: Address,
        sender: Address,
        holder: Address,
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
        sender: Address,
        recipient: Address,
        #[serde_as(as = "DisplayFromStr")]
        round: U256,
        #[serde_as(as = "DisplayFromStr")]
        released: U256,
        #[serde_as(as = "DisplayFromStr")]
        received: U256,
    },
    Referred {
        referrer_node: B256,
        binder: Address,
        session_id: B256,
        referrer_platform_id: B256,
        platform_id: B256,
        handle_node: B256,
        #[serde_as(as = "DisplayFromStr")]
        fee: U256,
        #[serde_as(as = "DisplayFromStr")]
        share: U256,
        outcome: Outcome,
    },
}

/// The wire's copy of a journaled event: the same fields, so a client reads
/// what the chain emitted, in a type the journal's format does not bind.
/// Disclosed text is the entry's [`HandleRef`], shown under the visibility
/// policy.
impl From<NamesEvent> for HistoryEvent {
    fn from(event: NamesEvent) -> Self {
        match event {
            NamesEvent::IdentityBound {
                holder,
                id_node,
                handle_node,
                platform_id,
                observed_at,
                ceremony_version,
            } => Self::IdentityBound {
                holder,
                id_node,
                handle_node,
                platform_id,
                observed_at,
                ceremony_version,
            },
            NamesEvent::HandleRevealed {
                platform_id,
                handle_node,
                handle: _,
            } => Self::HandleRevealed {
                platform_id,
                handle_node,
            },
            NamesEvent::HandlePublished {
                holder,
                platform_id,
                handle_node,
            } => Self::HandlePublished {
                holder,
                platform_id,
                handle_node,
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
            NamesEvent::HandleUnpublished { holder } => {
                Self::HandleUnpublished { holder }
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
                sender,
                round,
                amount,
                refundable_at,
            }) => Self::Deposited {
                handle_node,
                token,
                sender,
                round,
                amount,
                refundable_at,
            },
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                handle_node,
                token,
                sender,
                holder,
                amount,
                received,
            }) => Self::Forwarded {
                handle_node,
                token,
                sender,
                holder,
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
                sender,
                recipient,
                round,
                released,
                received,
            }) => Self::Refunded {
                handle_node,
                token,
                sender,
                recipient,
                round,
                released,
                received,
            },
            NamesEvent::Referred {
                referrer_node,
                binder,
                session_id,
                referrer_platform_id,
                platform_id,
                handle_node,
                fee,
                share,
                outcome,
            } => Self::Referred {
                referrer_node,
                binder,
                session_id,
                referrer_platform_id,
                platform_id,
                handle_node,
                fee,
                share,
                outcome,
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
    pub handle: HandleRef,
    /// Newest first: by block time, then chain, block and log index.
    pub entries: Vec<HistoryEntry>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
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

/// `GET /v1/escrow/claimable/{address}`: held for the handles the address
/// holds now, which `claim` pays it. `GET /v1/escrow/awaiting/{address}`:
/// held for those handles' nodes on chains where nobody holds them, which
/// binding the handle there makes claimable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressAmounts {
    /// The address asked about.
    pub address: Address,
    /// Claimable by chain, node and token; awaiting by node, chain and
    /// token; descending.
    pub amounts: Vec<EscrowAmount>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

/// One sender's contribution to a slot: what `refund` pays it, and from when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contribution {
    /// The amount, its slot and the slot's round.
    #[serde(flatten)]
    pub amount: EscrowAmount,
    /// When `refund` opens for it, in unix seconds: `REFUND_GRACE` after the
    /// sender's last deposit to the slot. Until then only the holder's claim
    /// moves it.
    pub refundable_at: i64,
}

/// `GET /v1/escrow/refundable/{address}`: what the address sent to handles
/// nobody has claimed yet, whether or not the handle has a holder, which
/// `refund` pays back from each contribution's date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressContributions {
    /// The address asked about.
    pub address: Address,
    /// By chain, node and token, descending.
    pub amounts: Vec<Contribution>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

/// `GET /v1/escrow/handle/{platform}/{handle}` and `GET /v1/escrow/node/{node}`:
/// what one handle holds, token by token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleAmounts {
    /// The handle, as far as the store knows it.
    pub handle: HandleRef,
    /// One per chain and token still holding something, descending.
    pub amounts: Vec<EscrowAmount>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

/// `GET /v1/escrow/unclaimed`: every slot still holding something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Unclaimed {
    /// The token the list was narrowed to, when one was asked for.
    pub token: Option<Address>,
    /// The page size served.
    pub limit: i64,
    /// Token by token, descending, the largest amount first within each.
    pub amounts: Vec<EscrowAmount>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

/// One bind that named a handle as its referrer, and what became of the
/// referrer's share of the fee.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Referral {
    /// The chain the bind happened on.
    pub chain_id: i64,
    /// Its block.
    pub block_number: i64,
    /// Its log's index in the block.
    pub log_index: i64,
    /// Its block's time, in unix seconds.
    pub block_time: i64,
    /// The address's handle the bind named as its referrer.
    pub referrer: HandleRef,
    /// The wallet that bound.
    pub binder: Address,
    /// The handle the binder publishes on that chain now, if any.
    pub binder_handle: Option<String>,
    /// The bind's fee, in the chain's coin units, as a decimal string.
    #[serde_as(as = "DisplayFromStr")]
    pub fee: U256,
    /// The referrer's share of it, likewise.
    #[serde_as(as = "DisplayFromStr")]
    pub share: U256,
    /// What became of the share.
    pub outcome: Outcome,
    /// For a paid share, where it went: forwarded to the handle's holder on
    /// that chain, or held for its claim, then claimed or refunded.
    pub delivery: Option<Delivery>,
}

/// `GET /v1/referrals/address/{address}`: the binds that named a handle the
/// address holds as their referrer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressReferrals {
    /// The address asked about.
    pub address: Address,
    /// Newest first: by block time, then chain, block and log index.
    pub referrals: Vec<Referral>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

/// `GET /v1/stats`: what the chains this store holds recorded, chain by
/// chain, and the users across them: totals and a series per UTC day.
/// Every address counts but a referral splitter any chain watches, which
/// sends every paid share: its deposits and forwards count, its wallet does
/// not. Amounts are token units, never converted. Tokens are named only when they are
/// the chain's own coin, EIP-7528 `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE`,
/// or a token the deployment lists (`STATS_TOKENS`); anybody can write a
/// token, so every other one is counted under `other`, without an amount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// Unix seconds, by the database's clock, of the snapshot every number
    /// comes from. The same answer is served for five minutes after it.
    pub computed_at: i64,
    /// The wallets across every chain the store holds: one that used
    /// several chains counts once.
    pub total: AllChains,
    /// The same, day by day, oldest first; a day with nothing recorded is
    /// absent.
    pub days: Vec<AllChainsDay>,
    /// Every chain the store holds, by id; a chain with nothing recorded
    /// yet included.
    pub chains: Vec<ChainStats>,
}

/// The wallets across every chain the store holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllChains {
    /// Distinct wallets that linked, sent through the escrow, or claimed
    /// from it, on any chain.
    pub users: i64,
}

/// One UTC day across every chain the store holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllChainsDay {
    /// Unix seconds of the day's start, 00:00 UTC.
    pub day: i64,
    /// Distinct wallets that day, on any chain.
    pub users: i64,
    /// Of them, those no chain had seen before.
    pub new_users: i64,
}

/// One chain's numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainStats {
    /// The chain.
    pub chain_id: i64,
    /// The handles that resolve now, per platform, by platform id.
    pub handles: Vec<PlatformHandles>,
    /// Everything the chain recorded since the index's first block.
    pub total: Activity,
    /// The same, day by day, oldest first; a day with nothing recorded is
    /// absent.
    pub days: Vec<DayActivity>,
}

/// The handles one platform has that resolve now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformHandles {
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// Handles with a holder: bound and never retired since.
    pub live: i64,
    /// Distinct wallets holding them.
    pub holders: i64,
}

/// What a chain recorded over a span: since the index's first block, or on
/// one day.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// Distinct wallets that linked, sent through the escrow, or claimed
    /// from it.
    pub users: i64,
    /// Bindings (`IdentityBound`) per platform, by platform id.
    pub links: Vec<PlatformLinks>,
    /// Registration fees paid (`BindFeePaid`) per receiver, by address.
    pub registration_fees: Vec<FeesPaid>,
    /// What moved through `HandleEscrow`.
    pub escrow: EscrowActivity,
    /// Claims of deposits made to a handle nobody held, claimed by the
    /// holder who bound it after the deposit: payments that brought their
    /// recipient in. The wallets are their claimers.
    pub onboarding_claims: TokenFlows,
}

/// One UTC day of a chain's activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayActivity {
    /// Unix seconds of the day's start, 00:00 UTC.
    pub day: i64,
    /// Of the day's users, those the chain had not seen before.
    pub new_users: i64,
    /// What the chain recorded that day.
    #[serde(flatten)]
    pub activity: Activity,
}

/// The bindings made on one platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformLinks {
    /// The platform's short key, when this build knows the id.
    pub platform: Option<KnownPlatform>,
    /// The platform id the chain keys by.
    pub platform_id: B256,
    /// Bindings made, a rebinding of an account to another wallet included.
    pub count: i64,
    /// Distinct wallets they bound to.
    pub holders: i64,
}

/// The registration fees one address was paid.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeesPaid {
    /// The fee receiver a binding named.
    pub receiver: Address,
    /// Fees paid.
    pub count: i64,
    /// Their sum in the chain's own coin, base units, as a decimal string.
    #[serde_as(as = "DisplayFromStr")]
    pub amount: U512,
}

/// What moved through `HandleEscrow`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EscrowActivity {
    /// Held for a handle nobody held (`Deposited`); the wallets are their
    /// senders.
    pub deposits: TokenFlows,
    /// Paid straight to a handle's holder (`Forwarded`); the wallets are
    /// their senders.
    pub forwards: TokenFlows,
    /// Taken by a holder (`Claimed`); the wallets are their claimers.
    pub claims: TokenFlows,
    /// Taken back by a sender (`Refunded`); the wallets are their senders.
    pub refunds: TokenFlows,
}

/// Movements of one kind: per named token, and every other token together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenFlows {
    /// The chain's own coin and the listed tokens, by token address.
    pub tokens: Vec<TokenFlow>,
    /// Every other token's movements, together; absent when there were
    /// none.
    pub other: Option<OtherTokens>,
}

/// One token's movements of one kind.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenFlow {
    /// The token; EIP-7528 `0xEeee…EEeE` is the chain's own coin.
    pub token: Address,
    /// Movements.
    pub count: i64,
    /// Their sum in token units, as a decimal string: what was sent, or
    /// what a claim or refund released. A sum, so it may pass a `uint256`.
    #[serde_as(as = "DisplayFromStr")]
    pub amount: U512,
    /// Distinct wallets that made them, as the list that holds this one
    /// names them.
    pub wallets: i64,
}

/// The movements of every token `/v1/stats` does not name, together: a
/// count and wallets, but no amount, since a sum across tokens means
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherTokens {
    /// Movements.
    pub count: i64,
    /// Distinct wallets that made them.
    pub wallets: i64,
}
