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
//! * **A chain the store does not hold** — mainnet included, and that is
//!   the case worth stating plainly, because `addr(node)` with no coin type
//!   IS a mainnet query and it is what `getAddress()` sends by default. The
//!   resolver carries ONE `urls` list for every query it ever answers — it
//!   cannot route by coin type — so ERC-3668 has the client walk that list
//!   until something succeeds. A signed null is a success, and it ends the
//!   walk. A gateway that signed null for every chain but its own would
//!   therefore answer, authoritatively and wrongly, for chains its neighbours
//!   in the list were there to serve. Refusing steps aside and lets the walk
//!   continue — which also means a deployment must index every chain it
//!   wants resolvable, mainnet among them, or the default query shape gets an
//!   error rather than an address.
//! * **A mirror too far behind.** Same shape: a signed null from a stale
//!   mirror denies a binding that may already exist.
//!
//! # Where an answer comes from
//!
//! The store, and nothing else. Every chain an indexer has written into it is
//! served, discovered per request rather than configured, so a new indexer is
//! answered for the first time it commits. An answer is as of whatever block
//! that indexer last committed — which is what [`Config::max_lag_blocks`] and
//! the indexer's own report guard.

use std::sync::Arc;

use alloy::{
    primitives::{
        Address,
        B256,
    },
    signers::Signer,
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
use serde::Serialize;
use tracing::{
    error,
    warn,
};
use usernames_core::{
    db::{
        ChainStore,
        MirrorPosition,
        Store,
    },
    ens::{
        AddrShape,
        CoinType,
        EnsError,
        KnownChain,
        Name,
        Record,
        Reply,
        ResolveCall,
        Subject,
    },
    nodes::NormalizedHandle,
};

/// What the gateway needs: the store it reads, and the identity it signs with.
#[derive(Clone)]
pub struct Config {
    /// The resolver this gateway answers for. Every answer is signed for this
    /// address, whatever `{sender}` the path carries: the target is
    /// configuration, never the request. A request naming another resolver is
    /// refused rather than answered, so a value that fell behind a
    /// `setResolver` is a visible 400 instead of a signature the resolver
    /// rejects.
    pub resolver: Address,
    /// The store every answer comes from. Which chains it holds is read per
    /// request, never configured: a coin type naming a chain no indexer has
    /// written gets an unsigned refusal, not a signed null — the resolver has
    /// one `urls` list for all chains, so asserting here would end a walk that
    /// another endpoint was there to finish.
    pub store: Store,
    /// How long an answer stays good. The resolver enforces it on chain.
    pub ttl_secs: u64,
    /// How far behind the chain a mirror may be and still assert anything.
    pub max_lag_blocks: u64,
    /// What signs an answer, pinned by the resolver's signer set; rotating it
    /// is an owner transaction there, not a deploy here.
    ///
    /// Behind `dyn` because the key need not be local. A KMS signer reaches the
    /// network to sign, so it satisfies only the async `Signer` — which is why
    /// the call site awaits rather than signing in place. That costs a local
    /// key nothing and leaves the signing path untouched when a deployment
    /// stops holding its own secret.
    pub signer: Arc<dyn Signer + Send + Sync>,
}

impl Config {
    /// Answer one forwarded call, or say why it cannot be answered.
    async fn answer(&self, call: &ResolveCall) -> Result<Answer, GatewayError> {
        // The NAME is settled before the record is looked at. Every answer
        // below is signed, and a signature makes it an assertion about a name
        // — so the name has to be one this resolver has standing to speak
        // about, whatever record was asked of it.
        let name = Name::parse(&call.name).map_err(GatewayError::bad_request)?;
        let query = name.query();
        // Not a name under this resolver's domain. Refused rather than
        // answered: a signed null is an authoritative "nobody holds this", and
        // this gateway has no standing to say that about someone else's name.
        if matches!(query, Err(EnsError::ForeignDomain)) {
            return Err(GatewayError::bad_request(
                "this resolver answers only for names under handles.link",
            ));
        }

        // Only `addr` has a shape this gateway can fill. Anything else —
        // `text()`, or a record invented later — degrades to no result rather
        // than to an error, so a name that resolves fine for addresses does
        // not look broken.
        //
        // Settled before any null is built, because a null has the shape of
        // the record it answers: `addr(bytes32)` returns an `address` and its
        // null is 32 zero bytes, while ENSIP-11's `addr(bytes32,uint256)`
        // returns `bytes` and its null is the empty string. A null built
        // before the record was read hard-coded the second shape, and a wallet
        // decoding the first read the `bytes` offset word as the address
        // 0x…0020 — signed.
        let Record::Addr {
            node,
            coin_type,
            shape,
        } = call.record
        else {
            return Ok(Answer::NoSuchRecord);
        };
        let null = Answer::Addr {
            address: None,
            shape,
        };

        // The request names one thing twice — as a name, and as the node
        // inside the record call — and only the caller has ever seen the two
        // agree. The signature covers both, so answering without checking
        // would sign a result that is true of the name and attributed to the
        // node.
        //
        // In practice this is a consistency check rather than a defence: one
        // caller builds both halves. What it earns is the guarantee the design
        // assumes elsewhere — that ENSIP-15 on the client and our own
        // normalization agree. Where they ever stop agreeing, this refuses
        // instead of signing.
        if name.node() != node {
            return Err(GatewayError::bad_request(
                "the name and the node in the record call are not the same name",
            ));
        }

        let (chain_id, mirror) = match self.mirror_for(coin_type).await? {
            ChainMatch::One { chain_id, mirror } => (chain_id, mirror),
            // A coin type that names no EVM chain at all — Bitcoin, say — is a
            // SIGNED null: no libID binding is ever an address of that kind,
            // and saying so needs no chain. Only an EVM chain the store does
            // not hold gets the refusal, because there a sibling gateway may
            // be the one that can answer.
            ChainMatch::None if coin_type.names_evm_chain() => {
                return Ok(Answer::NotOurChain(coin_type));
            }
            ChainMatch::None => return Ok(null),
            ChainMatch::Ambiguous => return Ok(Answer::Ambiguous(coin_type)),
        };

        // A name under our domain that this build cannot read is a name
        // nobody holds. Refusing would report an error for text a wallet is
        // entitled to ask about. Answered only here, past the chain check: a
        // chain this gateway does not serve keeps its unsigned refusal, so the
        // client walks on to a sibling that may read the name.
        let Ok(query) = query else {
            return Ok(null);
        };

        // A chain label narrows and never widens: a label naming another chain
        // is simply not this chain's name.
        if let Some(label) = &query.chain_label {
            if KnownChain::label_of(chain_id) != Some(label.as_str()) {
                return Ok(null);
            }
        }

        // Only now, with an answer actually owed, is staleness worth asking
        // about.
        if let Some(why) = mirror.lag().await.staleness(self.max_lag_blocks) {
            return Ok(Answer::TooStale(why));
        }

        let owner = match query.subject {
            Subject::Handle { platform, handle } => {
                // The inverse transform yields what `HandleNormalizer`
                // produced, so running the chain's rules over it again is
                // idempotent — and it is the only way to a `NormalizedHandle`,
                // which is what keeps a raw string from ever being hashed into
                // a key nobody finds.
                let Ok(normalized) = platform.normalize_query(&handle) else {
                    return Ok(null);
                };
                mirror.owner_of(platform.id(), &normalized).await?
            }
        };

        Ok(Answer::Addr {
            address: owner,
            shape,
        })
    }

    /// The one chain in the store a coin type names, if there is exactly one.
    ///
    /// Matched against the chains the STORE holds — whatever indexers have
    /// written — never decoded back into a chain id. `0x80000000 | chainId`
    /// is not injective past 2^31 (the eden testnet's 3735928814 is such a
    /// chain), so decoding would answer for a different chain, silently.
    /// Forwards the map is exact. Read per request, so a new indexer is served
    /// the first time it commits, and refused per request when two indexed
    /// chains share one coin type: answering either would be a guess, and a
    /// label does not rescue the pair — the coin type is what the wallet
    /// sends, and it is the coin type that is ambiguous.
    async fn mirror_for(&self, coin_type: CoinType) -> Result<ChainMatch, GatewayError> {
        let candidates: Vec<u64> = self
            .indexed_chains()
            .await?
            .into_iter()
            .filter(|id| coin_type.names_chain(*id))
            .collect();
        Ok(match candidates.as_slice() {
            [chain_id] => ChainMatch::One {
                chain_id: *chain_id,
                mirror: self.mirror(*chain_id),
            },
            [] => ChainMatch::None,
            both => {
                warn!(chains = ?both, %coin_type, "two indexed chains share one coin type");
                ChainMatch::Ambiguous
            }
        })
    }

    /// Every chain an indexer has written into the store.
    async fn indexed_chains(&self) -> Result<Vec<u64>, GatewayError> {
        Ok(self
            .store
            .indexed_chains()
            .await
            .map_err(GatewayError::internal)?
            .into_iter()
            .filter_map(|id| u64::try_from(id).ok())
            .collect())
    }

    /// One chain's mirror.
    fn mirror(&self, chain_id: u64) -> Mirror {
        Mirror(
            self.store
                .chain(i64::try_from(chain_id).expect("came from an i64")),
        )
    }

    /// Sign a result for one request, and encode what the resolver's callback
    /// decodes.
    async fn sign(
        &self,
        request: &[u8],
        result: Vec<u8>,
    ) -> Result<GatewayResponse, GatewayError> {
        let reply = Reply::good_for(result, self.ttl_secs);
        let signature = self
            .signer
            .sign_hash(&B256::from(reply.digest(self.resolver, request)))
            .await
            // Through `internal`, like every other failure here: the raw
            // error went to an anonymous caller while the operator got no log
            // line — exactly inverted.
            .map_err(GatewayError::internal)?;
        Ok(GatewayResponse {
            data: format!("0x{}", hex::encode(reply.encode(&signature.as_bytes()))),
        })
    }

    /// One row per chain the store holds, as supervision should see it.
    async fn status(&self) -> Result<GatewayStatus, GatewayError> {
        let indexed = self.indexed_chains().await?;
        let mut chains = Vec::with_capacity(indexed.len());
        for &chain_id in &indexed {
            let position = self.mirror(chain_id).position().await;
            let lag = Lag::of(position);
            // What the gate sees: a chain sharing its coin type with another
            // in the store is refused for that coin type however fresh either
            // is.
            let ambiguous = indexed
                .iter()
                .any(|other| CoinType::shared_by(chain_id, *other));
            chains.push(ChainStatus {
                chain_id,
                label: KnownChain::label_of(chain_id).map(str::to_string),
                lag_blocks: lag.blocks(),
                indexer_reported_at: position.and_then(|p| p.reported_at),
                report_valid_for: position.and_then(|p| p.valid_for),
                ambiguous,
                stale: ambiguous || lag.staleness(self.max_lag_blocks).is_some(),
            });
        }
        Ok(GatewayStatus { chains })
    }
}

/// Which chain a coin type named among the chains the store holds.
enum ChainMatch {
    /// Exactly one, and its mirror.
    One { chain_id: u64, mirror: Mirror },
    /// None: the store does not hold a chain with that coin type.
    None,
    /// Two, which share the coin type; neither may be answered.
    Ambiguous,
}

/// One chain's mirror: the indexed model an indexer keeps of it.
struct Mirror(ChainStore);

impl Mirror {
    /// The owner a name resolves to, as of whatever block this chain's
    /// indexer last committed. `None` is the null answer — a name nobody
    /// holds.
    async fn owner_of(
        &self,
        platform: B256,
        handle: &NormalizedHandle,
    ) -> Result<Option<Address>, GatewayError> {
        Ok(self
            .0
            .resolve_handle(platform, handle)
            .await
            .map_err(GatewayError::internal)?
            .and_then(|row| row.owner_address()))
    }

    /// Where this mirror stands, or nothing if the store cannot say.
    async fn position(&self) -> Option<MirrorPosition> {
        self.0.mirror_position().await.ok()
    }

    /// How far this mirror trails the chain.
    ///
    /// The position in one read, because this is the hot path and because the
    /// values must be a snapshot: the target, the cursor, and whether the
    /// indexer's report is still good — by the database's clock, the same one
    /// that stamped it.
    ///
    /// Against the TARGET, not the chain head: the indexer only ever advances
    /// the cursor to `head - CONFIRMATIONS`, so a caught-up mirror sits
    /// permanently that far behind the head, and measured that way a healthy
    /// deployment would refuse forever. And WHETHER the report is still good,
    /// because the target is the indexer's own claim: a loop that stopped
    /// leaves target and cursor frozen together, so by blocks alone a dead
    /// indexer reads as caught up for as long as it stays dead. The report it
    /// makes beside the target expires on its own schedule, and that notices.
    async fn lag(&self) -> Lag {
        Lag::of(self.position().await)
    }
}

/// How far behind the chain an answer would be.
enum Lag {
    /// Read from a mirror: this many blocks behind the target its indexer
    /// last set, and this many seconds before that indexer's report expires
    /// — negative once it has.
    Mirror { blocks: u64, valid_for: i64 },
    /// A mirror that cannot say. Treated as too stale, because a source that
    /// does not know its own position has not earned the right to deny a
    /// binding.
    Unknown,
}

impl Lag {
    /// The lag a mirror position amounts to. Missing pieces are `Unknown`,
    /// and unknown is refused: folding it into "no lag" is how a gateway ends
    /// up signing authoritative nulls off a wiped or unreachable database — a
    /// fresh deployment, a version-bump replay clearing
    /// `names.chain_metadata`, a chain nobody indexed yet, or Postgres simply
    /// being down all produce it.
    fn of(position: Option<MirrorPosition>) -> Self {
        match position {
            Some(MirrorPosition {
                target: Some(target),
                cursor: Some(cursor),
                valid_for: Some(valid_for),
                ..
            }) => Self::Mirror {
                blocks: target.saturating_sub(cursor),
                valid_for,
            },
            _ => Self::Unknown,
        }
    }

    /// Blocks behind the target, when the store could say.
    fn blocks(&self) -> Option<u64> {
        match self {
            Self::Mirror { blocks, .. } => Some(*blocks),
            Self::Unknown => None,
        }
    }

    /// Why a mirror in this position may not answer, if it may not. The one
    /// rule the gate and `/status` share, so supervision sees what the gate
    /// sees.
    fn staleness(&self, max_lag_blocks: u64) -> Option<Staleness> {
        match *self {
            Self::Mirror { valid_for, .. } if valid_for < 0 => {
                Some(Staleness::Expired(valid_for.unsigned_abs()))
            }
            Self::Mirror { blocks, .. } if blocks > max_lag_blocks => {
                Some(Staleness::Behind(blocks))
            }
            Self::Mirror { .. } => None,
            Self::Unknown => Some(Staleness::Unknown),
        }
    }
}

/// Why a mirror was refused an answer.
#[derive(Debug)]
enum Staleness {
    /// The cursor trails the target by this many blocks.
    Behind(u64),
    /// The indexer's last report expired this many seconds ago.
    Expired(u64),
    /// The mirror cannot report its position at all.
    Unknown,
}

impl Staleness {
    /// What the refusal tells the caller.
    fn message(&self) -> String {
        match self {
            Self::Behind(lag) => {
                format!("read model is {lag} blocks behind; not answering")
            }
            Self::Expired(secs) => {
                format!("the indexer's last report expired {secs}s ago; not answering")
            }
            Self::Unknown => {
                "read model cannot report its position; not answering".into()
            }
        }
    }
}

/// Either something to answer with — a null included, which is an answer — or
/// a refusal to assert anything at all.
enum Answer {
    /// An `addr` answer, and the shape to encode it in. `None` is the signed
    /// null: an authoritative "nobody holds this".
    Addr {
        address: Option<Address>,
        shape: AddrShape,
    },
    /// A record this gateway has no answer shape for. Distinct from a null,
    /// because a null has a shape and this has none.
    NoSuchRecord,
    /// The store holds no chain with that coin type. Unsigned, so the client
    /// walks on to the next endpoint in the resolver's `urls`.
    NotOurChain(CoinType),
    /// The store holds two chains with that coin type, so any answer would be
    /// a guess. Unsigned, for the same reason.
    Ambiguous(CoinType),
    /// This chain's mirror has not earned the right to deny a binding: too
    /// far behind, its indexer's report expired, or unable to say.
    TooStale(Staleness),
}

impl Answer {
    /// The bytes to sign, or the refusal to send instead.
    ///
    /// The refusals are unsigned on purpose: a signature would make the answer
    /// an assertion, and none of them has earned one. Unsigned lets the client
    /// fall through to the next endpoint in `urls`.
    fn into_result(self) -> Result<Vec<u8>, GatewayError> {
        match self {
            Self::Addr { address, shape } => Ok(shape.encode(address)),
            // Empty, not `addr`'s null. The caller decodes by ITS record's
            // return type, and `addr`'s empty `bytes` read as
            // `pubkey(bytes32)` or `interfaceImplementer` is a non-zero value
            // rather than an absence. Nothing decodes an empty result as
            // something.
            Self::NoSuchRecord => Ok(Vec::new()),
            Self::NotOurChain(coin_type) => Err(GatewayError::unavailable(format!(
                "this gateway serves no chain for coin type {coin_type}"
            ))),
            Self::Ambiguous(coin_type) => Err(GatewayError::unavailable(format!(
                "two indexed chains share coin type {coin_type}; not answering"
            ))),
            Self::TooStale(why) => {
                warn!(?why, "refusing to answer from a stale mirror");
                Err(GatewayError::unavailable(why.message()))
            }
        }
    }
}

/// State for the routes.
#[derive(Clone)]
pub struct GatewayState {
    // Behind an `Arc` so axum's per-request clone of the state copies one
    // pointer rather than a pool handle and a boxed signer.
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

/// Where the gateway is mounted, as the prefix the resolver's `urls` must
/// carry.
///
/// Under a prefix rather than at the root: `/{sender}/{data}` matches EVERY
/// two-segment path, so at the root it claims the whole shape for itself and
/// answers `/v1/typo` with "sender is not an address" instead of a 404 — and
/// silently swallows any future two-segment API route.
pub const ROUTE_PREFIX: &str = "/ens";

/// The ERC-3668 route, and the status beside it. `{sender}` and `{data}` are
/// the placeholders the resolver's `urls` carry; the `.json` suffix is
/// conventional and tolerated either way.
pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/{sender}/{data}", get(resolve))
        .route("/status", get(status))
        .with_state(state)
}

/// One served chain, as supervision should see it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChainStatus {
    chain_id: u64,
    label: Option<String>,
    /// Blocks between the indexer's target and its cursor.
    lag_blocks: Option<u64>,
    /// Unix seconds at which the indexer last reported.
    indexer_reported_at: Option<u64>,
    /// Seconds until the indexer's last report expires, negative once it has.
    report_valid_for: Option<i64>,
    /// Whether another chain in the store shares this chain's coin type, in
    /// which case queries for that coin type are refused however fresh either
    /// is: the store cannot say which was meant.
    ambiguous: bool,
    /// Whether a query for this chain would be refused right now, for any of
    /// the reasons above.
    stale: bool,
}

#[derive(Serialize)]
struct GatewayStatus {
    chains: Vec<ChainStatus>,
}

/// `GET /status`: one row per chain the store holds. For alerting, never for
/// readiness — a chain whose indexer stopped is refused on its own and the
/// others keep answering, so nothing here should take the process out of
/// rotation.
async fn status(
    State(state): State<GatewayState>,
) -> Result<Json<GatewayStatus>, GatewayError> {
    Ok(Json(state.config.status().await?))
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
    /// The caller's error: terminal for an ERC-3668 client, which does not
    /// walk on from a 4xx.
    fn bad_request(message: impl ToString) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.to_string(),
        }
    }

    /// An unsigned refusal: the client walks on to the next endpoint in the
    /// resolver's `urls`.
    fn unavailable(message: impl ToString) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.to_string(),
        }
    }

    /// A failure of ours. The cause is LOGGED, never serialized — the same
    /// split `api.rs` makes.
    ///
    /// A `sqlx` error carries schema names and connection strings, and this
    /// route is unauthenticated. Returning it would tell a caller things the
    /// REST half of the same process deliberately withholds, and returning it
    /// INSTEAD of logging it — which is what this used to do — leaves the
    /// operator with nothing while the caller has everything.
    fn internal(e: impl std::fmt::Display) -> Self {
        error!(cause = %e, "gateway lookup failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "lookup failed".into(),
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

/// The ERC-3668 route: check the sender, decode the call, answer, sign.
async fn resolve(
    State(state): State<GatewayState>,
    Path((sender, data)): Path<(String, String)>,
) -> Result<Json<GatewayResponse>, GatewayError> {
    let config = &state.config;
    let sender: Address = sender
        .parse()
        .map_err(|_| GatewayError::bad_request("sender is not an address"))?;
    // Bound to one resolver on purpose. The digest names the configured
    // target, never `sender`, so this check adds no authority — what it adds
    // is a visible error. Logged, because a 4xx is terminal for an ERC-3668
    // client (it ends the walk of the resolver's `urls`) and a line here is
    // the only way an operator learns that `ENS_RESOLVER_ADDRESS` fell behind
    // a `setResolver`.
    if sender != config.resolver {
        warn!(
            %sender,
            resolver = %config.resolver,
            "refusing a request that names another resolver"
        );
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
    let call = ResolveCall::decode(&call_data).map_err(GatewayError::bad_request)?;

    let result = config.answer(&call).await?.into_result()?;
    Ok(Json(config.sign(&call_data, result).await?))
}
