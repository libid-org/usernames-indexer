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
//! * **A chain this gateway does not serve** — mainnet included, and that is
//!   the case worth stating plainly, because `addr(node)` with no coin type
//!   IS a mainnet query and it is what `getAddress()` sends by default. The
//!   resolver carries ONE `urls` list for every query it ever answers — it
//!   cannot route by coin type — so ERC-3668 has the client walk that list
//!   until something succeeds. A signed null is a success, and it ends the
//!   walk. A gateway that signed null for every chain but its own would
//!   therefore answer, authoritatively and wrongly, for chains its neighbours
//!   in the list were there to serve. Refusing steps aside and lets the walk
//!   continue — which also means a deployment must list a gateway for every
//!   chain it wants resolvable, mainnet among them, or the default query
//!   shape gets an error rather than an address.
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
        U256,
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

alloy::sol! {
    /// `IdentityNames` rejects a platform it was never configured with.
    ///
    /// Declared here because the published bindings do not carry it, and the
    /// one place that must recognise it should name it rather than compare a
    /// selector literal.
    error UnknownPlatform(bytes32 platformId);
}
use serde::Serialize;
use tracing::{
    error,
    warn,
};
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
                let owner = match names
                    .resolveHandle(platform, handle.as_str().to_string())
                    .call()
                    .await
                {
                    Ok(owner) => owner,
                    // A platform this chain was never configured with is a
                    // name nobody can hold — the same fact the mirror reports
                    // as an empty row, and it deserves the same SIGNED null.
                    // Left as an error it became a 500, so `bob.github.…` on a
                    // chain wired only for `x` looked like an outage instead
                    // of an unclaimable name, and logged one line per request.
                    Err(e) if e.as_decoded_error::<UnknownPlatform>().is_some() => {
                        return Ok(None);
                    }
                    Err(e) => return Err(internal(e)),
                };
                // The contract answers the zero address for a name nobody
                // holds, which is the null answer rather than an address.
                Ok((!owner.is_zero()).then_some(owner))
            }
        }
    }

    /// How far this source trails the chain.
    ///
    /// Three answers, not two, and the third is the one that matters: reading
    /// the chain directly there IS no lag; reading a mirror it is a number; and
    /// reading a mirror whose head or cursor is missing or unreadable, it is
    /// UNKNOWN. Folding unknown into "no lag" is how a gateway ends up signing
    /// authoritative nulls off a wiped or unreachable database — a fresh
    /// deployment, a version-bump replay clearing `names.chain_metadata`, a
    /// chain listed in `ENS_CHAINS` that nobody indexed, or Postgres simply
    /// being down all produce it.
    async fn lag(&self) -> Lag {
        match self {
            Self::Chain { .. } => Lag::None,
            Self::Mirror(store) => {
                // Against the TARGET, not the chain head. The indexer records
                // the head as the chain's own height but only ever advances
                // the cursor to `head - CONFIRMATIONS`, so a fully caught-up
                // mirror is permanently that far behind the head. Measured
                // that way, any deployment whose CONFIRMATIONS reached
                // ENS_MAX_LAG_BLOCKS refused every ENS query forever while
                // being perfectly healthy — two knobs in two binaries with
                // nothing relating them.
                let (Ok(Some(target)), Ok(Some(cursor))) =
                    (store.chain_target().await, store.cursor().await)
                else {
                    return Lag::Unknown;
                };
                Lag::Blocks(target.saturating_sub(cursor))
            }
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
    // Behind an `Arc` because axum clones the state per request, and this map
    // holds a `ChainStore` and a provider per chain. `signer` was already
    // `Arc` for the same reason; this finishes the job.
    config: Arc<Config>,
}

impl GatewayState {
    /// Wire the gateway to its configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Where the gateway is mounted, as the prefix the resolver's `urls` must
/// carry.
///
/// Under a prefix rather than at the root: `/{sender}/{data}` matches EVERY
/// two-segment path, so at the root it claims the whole shape for itself and
/// answers `/v1/typo` with "sender is not an address" instead of a 404 — and
/// silently swallows any future two-segment API route.
pub const ROUTE_PREFIX: &str = "/ens";

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
        Answer::NotOurChain(coin_type) => {
            return Err(GatewayError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!(
                    "this gateway serves no chain for coin type {coin_type}"
                ),
            });
        }
        Answer::TooStale { lag } => {
            warn!(?lag, "refusing to answer from a stale mirror");
            return Err(GatewayError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: match lag {
                    Some(lag) => {
                        format!("read model is {lag} blocks behind; not answering")
                    }
                    None => "read model cannot report its position; not answering".into(),
                },
            });
        }
    };

    let result = ens::encode_addr_result(record, address);
    let expires = unix_now() + state.config.ttl_secs;
    let digest =
        ens::signature_digest(state.config.resolver, expires, &call_data, &result);
    let signature = state
        .config
        .signer
        .sign_hash_sync(&B256::from(digest))
        // Through `internal`, like every other failure here: the raw error
        // went to an anonymous caller while the operator got no log line —
        // exactly inverted.
        .map_err(internal)?;

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

/// How far behind the chain an answer would be.
enum Lag {
    /// Read from the chain: the question does not apply.
    None,
    /// Read from a mirror, this far behind.
    Blocks(u64),
    /// A mirror that cannot say. Treated as too stale, because a source that
    /// does not know its own position has not earned the right to deny a
    /// binding.
    Unknown,
}

/// Either the address to answer with — `None` meaning a signed null — or a
/// refusal to assert anything at all.
enum Answer {
    Address(Option<Address>),
    /// This gateway serves no chain with that coin type. Unsigned, so the
    /// client walks on to the next endpoint in the resolver's `urls`.
    NotOurChain(U256),
    /// This chain's mirror is too far behind to deny a binding, or cannot say
    /// how far behind it is.
    TooStale {
        lag: Option<u64>,
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
    let Record::Addr { coin_type, .. } = record else {
        return Ok(Answer::Address(None));
    };
    // Matched against the chains this gateway SERVES, never decoded back into
    // a chain id. `0x80000000 | chainId` is not injective past 2^31 — the eden
    // testnet's 3735928814 is exactly such a chain — so decoding would answer
    // for a different chain, silently. Forwards the map is exact, and startup
    // refuses a configuration where two chains share one coin type.
    //
    // Mainnet is looked up twice because it answers to two coin types: the
    // legacy 60, and ENSIP-11's `0x80000001`.
    let matched = state
        .config
        .chains
        .iter()
        .find(|(id, _)| ens::coin_type_for(**id) == coin_type)
        .map(|(_, chain)| chain)
        .or_else(|| {
            ens::is_mainnet_coin_type(coin_type)
                .then(|| state.config.chains.get(&1))
                .flatten()
        });
    let Some(chain) = matched else {
        // A coin type that names no EVM chain at all — Bitcoin, say — is a
        // SIGNED null: no libID binding is ever an address of that kind, and
        // saying so needs no chain. Only an EVM chain we do not serve gets the
        // refusal, because there a sibling gateway may be the one that can
        // answer.
        return Ok(if ens::names_an_evm_chain(coin_type) {
            Answer::NotOurChain(coin_type)
        } else {
            Answer::Address(None)
        });
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

    // Only now, with an answer actually owed, is staleness worth asking about.
    match chain.source.lag().await {
        Lag::None => {}
        Lag::Blocks(lag) if lag <= state.config.max_lag_blocks => {}
        Lag::Blocks(lag) => return Ok(Answer::TooStale { lag: Some(lag) }),
        Lag::Unknown => return Ok(Answer::TooStale { lag: None }),
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
    };

    Ok(Answer::Address(owner))
}

/// The cause is LOGGED, never serialized — the same split `api.rs` makes.
///
/// A `sqlx` or RPC error carries schema names, connection strings and provider
/// detail, and this route is unauthenticated. Returning it would tell a caller
/// things the REST half of the same process deliberately withholds, and
/// returning it INSTEAD of logging it — which is what this used to do — leaves
/// the operator with nothing while the caller has everything.
fn internal(e: impl std::fmt::Display) -> GatewayError {
    error!(cause = %e, "gateway lookup failed");
    GatewayError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "lookup failed".into(),
    }
}
