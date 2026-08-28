//! The CCIP-Read gateway: one route, and the key that signs its answers.
//!
//! ERC-3668 puts the endpoint inside the resolver's revert, so a client
//! arrives here already knowing which resolver it is talking to and what it
//! asked. This turns that into a question for the read model, signs the
//! answer, and hands it back. Nothing is registered anywhere and no state is
//! kept; two on-chain `eth_call`s bracket this and both are free.
//!
//! # What is signed, and what is refused
//!
//! A name nobody holds gets a SIGNED null. That is not a technicality: a
//! wallet has to be able to trust "nobody holds this" exactly as much as it
//! trusts an address, or every unclaimed name looks like an outage.
//!
//! Two other cases get an UNSIGNED error instead, and for the same reason: a
//! signature turns an answer into an assertion, and neither of these has
//! earned one.
//!
//! * **A chain this gateway does not serve.** The resolver carries ONE `urls`
//!   list for every query it ever answers — it cannot route by coin type — so
//!   ERC-3668 has the client walk that list until something succeeds. A signed
//!   null is a success, and it ends the walk. A gateway that signed null for
//!   every chain but its own would therefore answer, authoritatively and
//!   wrongly, for chains its neighbours in the list were there to serve.
//!   Refusing steps aside and lets the walk continue.
//! * **A mirror too far behind.** Same shape: a signed null from a stale
//!   mirror denies a binding that may already exist.
//!
//! # Where an answer comes from
//!
//! [`HandleSource`] is one lookup wide and has two implementations. The mirror
//! answers from Postgres, cheaply, as of whatever block the indexer last
//! committed. The chain answers over RPC, as of the head. They differ in what
//! they can honestly assert, which is why the second needs no lag gate — and
//! why it is the stronger choice for a signed answer, whatever the default
//! says.

use std::{
    collections::HashMap,
    sync::Arc,
};

use alloy::{
    primitives::{
        Address,
        B256,
    },
    providers::RootProvider,
    signers::{
        local::PrivateKeySigner,
        SignerSync,
    },
};
use axum::{
    extract::{
        Path,
        State,
    },
    http::StatusCode,
    response::{
        IntoResponse,
        Response,
    },
    routing::get,
    Json,
    Router,
};
use libid_contracts::bindings::identity::IdentityNames;
use serde::Serialize;
use tracing::warn;
use usernames_core::{
    db::ChainStore,
    ens::{
        self,
        Record,
        Subject,
    },
    nodes::NormalizedHandle,
};

/// Where one chain's answers come from.
///
/// One lookup wide, and closed: an enum rather than a trait because there are
/// exactly two and dynamic dispatch would buy nothing.
#[derive(Clone)]
pub enum HandleSource {
    /// The indexed read model. Cheap, and as of whatever block the indexer
    /// last committed — which is what [`Config::max_lag_blocks`] guards.
    Mirror(ChainStore),
    /// The contract itself, as of the head. No lag to guard, and no `_id`
    /// form: `IdentityNames` exposes no getter keyed by `idNode`, only the
    /// event, so a node-addressed name has nothing to ask.
    Chain {
        /// The JSON-RPC endpoint for this chain.
        provider: RootProvider,
        /// `IdentityNames`, which sits at one CREATE3 address on every chain.
        contract: Address,
    },
}

impl HandleSource {
    async fn resolve_handle(
        &self,
        platform: B256,
        handle: &NormalizedHandle,
    ) -> Result<Option<Address>, GatewayError> {
        match self {
            Self::Mirror(store) => Ok(store
                .resolve_handle(platform, handle)
                .await
                .map_err(internal)?
                .and_then(|row| row.owner)
                .as_deref()
                .and_then(as_address)),
            Self::Chain { provider, contract } => {
                let names = IdentityNames::new(*contract, provider);
                let owner = names
                    .resolveHandle(platform, handle.as_str().to_string())
                    .call()
                    .await
                    .map_err(internal)?;
                // The contract answers the zero address for a name nobody
                // holds, which is the null answer rather than an address.
                Ok((!owner.is_zero()).then_some(owner))
            }
        }
    }

    async fn resolve_id_node(&self, node: B256) -> Result<Option<Address>, GatewayError> {
        match self {
            Self::Mirror(store) => Ok(store
                .resolve_by_id_node(node)
                .await
                .map_err(internal)?
                .map(|row| row.owner)
                .as_deref()
                .and_then(as_address)),
            // Not a refusal: the id-derived name cannot be encoded in DNS wire
            // format at all (64 hex characters is one past the label ceiling),
            // so nothing can ask this and a null is the honest answer.
            Self::Chain { .. } => Ok(None),
        }
    }

    /// How far this source trails the chain, when that question means
    /// anything. Reading the chain directly, it does not.
    async fn lag_blocks(&self) -> Option<u64> {
        match self {
            Self::Mirror(store) => {
                let head = store.chain_head().await.ok().flatten()?;
                let cursor = store.cursor().await.ok().flatten()?;
                Some(head.saturating_sub(cursor))
            }
            Self::Chain { .. } => None,
        }
    }
}

fn as_address(bytes: &[u8]) -> Option<Address> {
    <[u8; 20]>::try_from(bytes).ok().map(Address::from)
}

/// One chain this gateway answers for.
#[derive(Clone)]
pub struct ChainGateway {
    /// This chain's label in the name hierarchy, when it has one. A name
    /// carrying a different label is not this chain's name.
    pub label: Option<String>,
    /// Where its answers come from.
    pub source: HandleSource,
}

/// What the gateway needs beyond the chains it serves.
#[derive(Clone)]
pub struct Config {
    /// The resolver this gateway answers for. An answer is bound to it by the
    /// signature, and a request naming another resolver is refused rather
    /// than signed — otherwise this is a signing oracle for any contract that
    /// cares to ask.
    pub resolver: Address,
    /// Every chain this gateway can speak for, by chain id. A coin type
    /// naming anything else gets an unsigned refusal, not a signed null: the
    /// resolver has one `urls` list for all chains, so asserting here would
    /// end a walk that another endpoint was there to finish.
    pub chains: HashMap<u64, ChainGateway>,
    /// How long an answer stays good. The resolver enforces it on chain.
    pub ttl_secs: u64,
    /// How far behind the chain a MIRROR may be and still assert anything.
    /// Meaningless for a chain-backed source, which has no lag.
    pub max_lag_blocks: u64,
    /// The signing key. One secret, pinned by the resolver's signer set;
    /// rotating it is an owner transaction there, not a deploy here.
    pub signer: Arc<PrivateKeySigner>,
}

/// State for the one route.
#[derive(Clone)]
pub struct GatewayState {
    config: Config,
    /// Injected so a test can pin the expiry instead of racing the clock.
    now: fn() -> u64,
}

impl GatewayState {
    /// Wire the gateway to its configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            now: unix_now,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The ERC-3668 route. `{sender}` and `{data}` are the placeholders the
/// resolver's `urls` carry; the `.json` suffix is conventional and tolerated
/// either way.
pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/{sender}/{data}", get(resolve))
        .with_state(state)
}

/// What ERC-3668 hands back: one hex blob the resolver's callback decodes.
#[derive(Serialize)]
struct GatewayResponse {
    data: String,
}

/// Why an answer could not be given at all — as opposed to given as null.
struct GatewayError {
    status: StatusCode,
    message: String,
}

impl GatewayError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "message": self.message })),
        )
            .into_response()
    }
}

async fn resolve(
    State(state): State<GatewayState>,
    Path((sender, data)): Path<(String, String)>,
) -> Result<Json<GatewayResponse>, GatewayError> {
    let sender: Address = sender
        .parse()
        .map_err(|_| GatewayError::bad_request("sender is not an address"))?;
    // Bound to one resolver on purpose. The signature names the target, so
    // signing for a caller-supplied one would let any contract borrow this
    // key's authority.
    if sender != state.config.resolver {
        return Err(GatewayError::bad_request(
            "this gateway answers for a different resolver",
        ));
    }

    let call_data = hex::decode(
        data.strip_suffix(".json")
            .unwrap_or(&data)
            .trim_start_matches("0x"),
    )
    .map_err(|_| GatewayError::bad_request("data is not hex"))?;

    let (name, record) = ens::decode_resolve_calldata(&call_data)
        .map_err(|e| GatewayError::bad_request(e.to_string()))?;

    let address = match self_answer(&state, &name, record).await? {
        Answer::Address(address) => address,
        // Both of these are unsigned on purpose: a signature would make the
        // answer an assertion, and neither case has earned one. Unsigned lets
        // the client fall through to the next endpoint in `urls`.
        Answer::NotOurChain(chain_id) => {
            return Err(GatewayError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("this gateway does not serve chain {chain_id}"),
            });
        }
        Answer::TooStale { lag } => {
            warn!(lag, "refusing to answer from a stale mirror");
            return Err(GatewayError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("read model is {lag} blocks behind; not answering"),
            });
        }
    };

    let result = ens::encode_addr_result(record, address);
    let expires = (state.now)() + state.config.ttl_secs;
    let digest =
        ens::signature_digest(state.config.resolver, expires, &call_data, &result);
    let signature = state
        .config
        .signer
        .sign_hash_sync(&B256::from(digest))
        .map_err(|e| GatewayError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("signing failed: {e}"),
        })?;

    Ok(Json(GatewayResponse {
        data: format!(
            "0x{}",
            hex::encode(ens::encode_response(
                &result,
                expires,
                &signature.as_bytes()
            ))
        ),
    }))
}

/// Either the address to answer with — `None` meaning a signed null — or a
/// refusal to assert anything at all.
enum Answer {
    Address(Option<Address>),
    /// This gateway does not serve that chain. Unsigned, so the client walks
    /// on to the next endpoint in the resolver's `urls`.
    NotOurChain(u64),
    /// This chain's mirror is too far behind to deny a binding.
    TooStale {
        lag: u64,
    },
}

async fn self_answer(
    state: &GatewayState,
    name: &[u8],
    record: Record,
) -> Result<Answer, GatewayError> {
    // Any record other than `addr` is null rather than an error, so a client
    // asking for `text()` or something invented later degrades instead of
    // seeing a name that resolves report a failure.
    let Record::Addr { chain_id, .. } = record else {
        return Ok(Answer::Address(None));
    };
    // A coin type naming no EVM chain — Bitcoin, say — is a signed null: no
    // libID binding is ever an address of that kind, and that is knowable
    // without serving any chain.
    let Some(chain_id) = chain_id else {
        return Ok(Answer::Address(None));
    };
    // A chain this gateway does not serve is the one case that must NOT be
    // signed. The resolver holds one `urls` list for every query, so a signed
    // null here ends a walk that a sibling endpoint was there to finish.
    let Some(chain) = state.config.chains.get(&chain_id) else {
        return Ok(Answer::NotOurChain(chain_id));
    };

    let labels = ens::parse_dns_name(name)
        .map_err(|e| GatewayError::bad_request(e.to_string()))?;
    let query = match ens::parse_query(&labels) {
        Ok(query) => query,
        // A name this build cannot read is a name nobody holds. Refusing
        // would report an error for text a wallet is entitled to ask about.
        Err(_) => return Ok(Answer::Address(None)),
    };
    // A chain label narrows and never widens: a label naming another chain is
    // simply not this chain's name.
    if let Some(label) = &query.chain_label {
        if chain.label.as_deref() != Some(label.as_str()) {
            return Ok(Answer::Address(None));
        }
    }

    // Only now, with an answer actually owed, is staleness worth asking about
    // — and only a mirror has any.
    if let Some(lag) = chain.source.lag_blocks().await {
        if lag > state.config.max_lag_blocks {
            return Ok(Answer::TooStale { lag });
        }
    }

    let owner = match query.subject {
        Subject::Handle { platform, handle } => {
            // The inverse transform yields what `HandleNormalizer` produced,
            // so running the chain's rules over it again is idempotent — and
            // it is the only way to a `NormalizedHandle`, which is what keeps
            // a raw string from ever being hashed into a key nobody finds.
            let Ok(normalized) = platform.normalize_query(&handle) else {
                return Ok(Answer::Address(None));
            };
            chain
                .source
                .resolve_handle(platform.id(), &normalized)
                .await?
        }
        Subject::IdNode(node) => chain.source.resolve_id_node(node).await?,
    };

    Ok(Answer::Address(owner))
}

fn internal(e: impl std::fmt::Display) -> GatewayError {
    GatewayError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("read model unavailable: {e}"),
    }
}
