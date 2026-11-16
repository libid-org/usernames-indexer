//! ENS: turning a name under `handles.link` back into a question the
//! read model can answer.
//!
//! Everything here is pure. It parses, it inverts, it encodes; it reads no
//! database, holds no key and signs nothing. The route that does those things
//! lives in the API binary, which is also where the signing dependency stays
//! so the indexer never grows one.
//!
//! # The shape
//!
//! ```text
//! <handle labels> . <platform> [ . <chain> ] . handles . link
//! ```
//!
//! The parse runs right to left, because a Gmail local part contributes a
//! variable number of labels. Platform keys are a closed set and a chain name
//! is never one of them — [`ChainName`] refuses it where a deployment declares
//! it — which is what keeps the parse unambiguous. Which chains have names,
//! and which, is the store's to say: each chain's indexer writes its own.
//!
//! # Which chain
//!
//! A coin type names one chain, and none of them is a default. Bare
//! `addr(node)` is coin type 60: the coin of the chain whose ENS registry was
//! asked, so Ethereum mainnet through the mainnet registry and Sepolia
//! through Sepolia's. Whether an answer is owed for it is the gateway's
//! decision rather than this module's — see `usernames-api`'s `ens` module,
//! where a chain the store does not hold is refused rather than denied.
//!
//! Note what the range limit does and does not cost. `0x80000000 | chainId`
//! stops being injective at 2^31, so a coin type cannot be decoded back to one
//! chain id — but the forward direction is always well defined, and a chain
//! past 2^31 (the eden testnet's 3735928814 is one) is addressable like any
//! other. That is why [`CoinType::of_chain`] is the only direction offered,
//! and why a gateway matches a query against the chains its store holds
//! rather than decoding. [`CoinType::shared_by`] names the pair that cannot
//! be served together.

use std::{
    fmt,
    str::FromStr,
};

use alloy::{
    primitives::{
        keccak256,
        Address,
        Bytes,
        B256,
        U256,
    },
    sol_types::SolValue,
};

use crate::nodes::KnownPlatform;

/// The domain a gateway answers under when its deployment names none.
pub const DEFAULT_DOMAIN: &str = "handles.link";

/// The domain a gateway's names sit under: `handles.link`, or
/// `testnet.handles.link` for a deployment that answers under a subname.
///
/// A deployment declares it; a name outside it is foreign, whatever its
/// labels look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    labels: Vec<String>,
}

impl Domain {
    /// The labels, leftmost first: `["testnet", "handles", "link"]`.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }
}

impl FromStr for Domain {
    type Err = EnsError;

    /// Dotted labels, each one a wallet could send after ENS normalization.
    fn from_str(dotted: &str) -> Result<Self, Self::Err> {
        let labels: Vec<String> = dotted.split('.').map(String::from).collect();
        match labels.iter().find(|l| !Name::label_is_wellformed(l)) {
            Some(label) => Err(EnsError::UnnormalizedLabel(label.clone())),
            None => Ok(Self { labels }),
        }
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.labels.join("."))
    }
}

/// A name a chain goes by in a name: the `base` in `alice.x.base.handles.link`.
///
/// Checked here, once, where a deployment declares it, so the store only ever
/// holds names the parse can read back: a label a wallet could send, and
/// never a platform key — the parse decides "platform or chain" by the
/// platform keys, so an overlap would make one name mean two things.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainName(String);

/// Why a string cannot name a chain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainNameError {
    /// Not a label: lowercase ASCII letters, digits and hyphens only.
    #[error("chain name {0:?} is not a label: lowercase ascii letters, digits and hyphens only")]
    Malformed(String),
    /// A platform key, which the parse would read as the platform.
    #[error("chain name {0:?} is a platform key")]
    Platform(String),
}

/// A name a chain may go by.
impl FromStr for ChainName {
    type Err = ChainNameError;

    fn from_str(name: &str) -> Result<Self, ChainNameError> {
        if !Name::label_is_wellformed(name) {
            return Err(ChainNameError::Malformed(name.to_string()));
        }
        if name.parse::<KnownPlatform>().is_ok() {
            return Err(ChainNameError::Platform(name.to_string()));
        }
        Ok(Self(name.to_string()))
    }
}

impl fmt::Display for ChainName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ChainName {
    /// The name, as the label it appears as.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// ENSIP-11: an EVM chain's coin type is `0x80000000 | chainId`.
const EVM_COIN_TYPE_BIT: u64 = 0x8000_0000;

/// Coin type 60 is ether: the coin of the chain whose ENS registry was asked —
/// a specific chain, not an unknown one.
const COIN_TYPE_ETH: u64 = 60;

/// A coin type, exactly as a wallet asked with it: undecoded, and matched
/// forward against chains rather than turned back into a chain id.
///
/// The forward direction is total and unambiguous for every chain id. It is
/// the REVERSE that is lossy: above 2^31 the bit is already set, the OR
/// changes nothing, and two chain ids land on one coin type. So a gateway
/// asks [`Self::names_chain`] of each chain the store holds rather than
/// computing a chain id back out — which would have to guess, and would guess
/// wrong for any id past 2^31. Two indexed chains that collide are refused per
/// request, unsigned, for the coin type they share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoinType(U256);

impl CoinType {
    /// The coin type ENSIP-11 gives an EVM chain.
    pub fn of_chain(chain_id: u64) -> Self {
        Self(U256::from(EVM_COIN_TYPE_BIT | chain_id))
    }

    /// Whether this coin type names an EVM chain at all.
    ///
    /// A coin type without the ENSIP-11 bit belongs to another kind of chain
    /// entirely — Bitcoin is 0 — and no libID binding is ever an address of
    /// that kind. That is knowable without serving any chain, which is why
    /// such a query earns a signed null rather than a refusal.
    pub fn names_evm_chain(self) -> bool {
        let Ok(raw) = u64::try_from(self.0) else {
            return false;
        };
        raw == COIN_TYPE_ETH || raw & EVM_COIN_TYPE_BIT != 0
    }

    /// Whether this coin type, asked through the ENS registry on `ens_chain`,
    /// names the given chain — forward, never decoded.
    ///
    /// The registry's own chain answers to two: its ENSIP-11 coin type, and
    /// the legacy 60. A wallet on Sepolia asks Sepolia's registry with bare
    /// `addr(node)` and sends what it is given on Sepolia, so 60 there is
    /// Sepolia and never Ethereum mainnet.
    pub fn names_chain(self, chain_id: u64, ens_chain: u64) -> bool {
        Self::of_chain(chain_id) == self
            || (chain_id == ens_chain && self.0 == U256::from(COIN_TYPE_ETH))
    }

    /// Whether two chains cannot be told apart by their coin type.
    pub fn shared_by(a: u64, b: u64) -> bool {
        a != b && Self::of_chain(a) == Self::of_chain(b)
    }
}

impl From<U256> for CoinType {
    fn from(raw: U256) -> Self {
        Self(raw)
    }
}

impl fmt::Display for CoinType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// `resolve(bytes,bytes)`, the ENSIP-10 entry point.
pub const RESOLVE_SELECTOR: [u8; 4] = [0x90, 0x61, 0xb9, 0x23];

/// `addr(bytes32)` — the legacy form, coin type 60 by definition.
const ADDR_SELECTOR: [u8; 4] = [0x3b, 0x3b, 0x57, 0xde];

/// `addr(bytes32,uint256)` — ENSIP-11.
pub const ADDR_COIN_SELECTOR: [u8; 4] = [0xf1, 0xcb, 0x7e, 0x06];

/// Why a request could not be understood at all.
///
/// Distinct from "understood, and the answer is null": a malformed request is
/// the caller's error and gets a 4xx, while a name nobody holds is a perfectly
/// good question with an empty answer, and must be SIGNED — a wallet has to be
/// able to trust "nobody holds this" as much as it trusts an address.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnsError {
    /// The DNS wire encoding is truncated, unterminated, or uses a length no
    /// label may have.
    #[error("malformed DNS-encoded name")]
    MalformedName,
    /// A label carried bytes outside what ENS normalization can produce.
    #[error("label {0:?} is not valid ENS-normalized text")]
    UnnormalizedLabel(String),
    /// The name does not sit under this gateway's domain.
    #[error("name is not under this gateway's domain")]
    ForeignDomain,
    /// Nothing left after the domain, or a platform label with no handle.
    #[error("name has no subject beneath the domain")]
    EmptyName,
    /// The outer call is not `resolve(bytes,bytes)`.
    #[error("call is not resolve(bytes,bytes)")]
    NotResolve,
    /// The outer call's arguments do not decode.
    #[error("malformed resolve(bytes,bytes) arguments")]
    MalformedCallData,
}

/// What the name asks about, once the labels are inverted.
#[derive(Debug, Clone)]
pub enum Subject {
    /// A handle on a platform, already in the byte form the chain keyed.
    Handle {
        /// The platform the labels named.
        platform: KnownPlatform,
        /// The handle the labels invert to.
        handle: String,
    },
}

/// A parsed name: what it asks about, and whether it narrows to a chain.
#[derive(Debug, Clone)]
pub struct Query {
    /// The account the name names.
    pub subject: Subject,
    /// The chain label, when the name carries one. A name with a label is
    /// answered only for that chain; a name without one is answered by the
    /// caller's coin type alone. A label never widens and never overrides.
    pub chain_label: Option<String>,
}

/// The shape an `addr` answer takes: the one the caller asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrShape {
    /// `addr(bytes32)`, which returns a bare `address`.
    Legacy,
    /// ENSIP-11's `addr(bytes32,uint256)`, which returns `bytes`.
    Multichain,
}

impl AddrShape {
    /// ABI-encode an `addr` answer in this shape.
    ///
    /// `None` is the null answer, and it is a real answer rather than an
    /// absence: the multichain form returns `bytes`, so null is the empty
    /// byte string, and the legacy form returns the zero address.
    pub fn encode(self, address: Option<Address>) -> Vec<u8> {
        match self {
            Self::Legacy => address.unwrap_or_default().abi_encode(),
            Self::Multichain => {
                Bytes::from(address.map(|a| a.to_vec()).unwrap_or_default()).abi_encode()
            }
        }
    }
}

/// The record the client actually asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// `addr(node)` or `addr(node, coinType)`.
    Addr {
        /// The node the caller named, for checking against the name that
        /// arrived beside it. Both halves of the request describe one name,
        /// and only the caller has ever seen them agree.
        node: B256,
        /// Exactly what the caller asked with, undecoded. Matching it against
        /// the chains the store holds is the gateway's job.
        coin_type: CoinType,
        /// The form the caller used, which is the form the answer takes.
        shape: AddrShape,
    },
    /// Any other record. Answered null rather than refused, so a client asking
    /// for `text()` or a future record type degrades instead of erroring.
    Other,
}

impl Record {
    /// Which record the inner calldata asks for.
    fn decode(inner: &[u8]) -> Self {
        let Some((selector, args)) = inner.split_at_checked(4) else {
            return Self::Other;
        };
        match selector {
            // addr(bytes32) — the node and nothing else, which is coin type 60
            // by definition: the coin of the chain whose registry was asked.
            s if s == ADDR_SELECTOR => match <(B256,)>::abi_decode_params(args) {
                Ok((node,)) => Self::Addr {
                    node,
                    coin_type: CoinType::from(U256::from(COIN_TYPE_ETH)),
                    shape: AddrShape::Legacy,
                },
                Err(_) => Self::Other,
            },
            // addr(bytes32,uint256) — the coin type carried through exactly as
            // sent.
            s if s == ADDR_COIN_SELECTOR => match <(B256, U256)>::abi_decode_params(args)
            {
                Ok((node, coin_type)) => Self::Addr {
                    node,
                    coin_type: CoinType::from(coin_type),
                    shape: AddrShape::Multichain,
                },
                Err(_) => Self::Other,
            },
            _ => Self::Other,
        }
    }
}

/// The `resolve(bytes name, bytes data)` call the resolver forwarded: the
/// DNS-encoded name, and the record it wraps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveCall {
    /// The name, in DNS wire format, exactly as the caller sent it.
    pub name: Vec<u8>,
    /// The record the inner call asks for.
    pub record: Record,
}

impl ResolveCall {
    /// Decode the forwarded call.
    pub fn decode(call_data: &[u8]) -> Result<Self, EnsError> {
        let (selector, body) =
            call_data.split_at_checked(4).ok_or(EnsError::NotResolve)?;
        if selector != RESOLVE_SELECTOR {
            return Err(EnsError::NotResolve);
        }
        let (name, inner) = <(Bytes, Bytes)>::abi_decode_params(body)
            .map_err(|_| EnsError::MalformedCallData)?;
        Ok(Self {
            name: name.into(),
            record: Record::decode(&inner),
        })
    }
}

/// A name, as its labels.
///
/// Kept as the list the wire format gave rather than a joined string: a DNS
/// wire label may itself contain a dot — nothing in the format forbids it —
/// so `["a.b"]` and `["a", "b"]` are two names, and only the list tells them
/// apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    labels: Vec<String>,
}

impl Name {
    /// Decode a DNS wire-format name into its labels.
    ///
    /// `\x05alice\x01x\x07handles\x04link\x00` becomes `["alice", "x",
    /// "handles", "link"]`. The root label terminates; trailing bytes after it
    /// are a malformed name rather than something to ignore.
    pub fn decode(wire: &[u8]) -> Result<Self, EnsError> {
        let mut labels = Vec::new();
        let mut at = 0usize;
        loop {
            let len = *wire.get(at).ok_or(EnsError::MalformedName)? as usize;
            at += 1;
            if len == 0 {
                break;
            }
            // 63 is the DNS label ceiling; the two high bits are compression
            // pointers, which have no meaning in a name passed as calldata.
            if len > 63 {
                return Err(EnsError::MalformedName);
            }
            let end = at.checked_add(len).ok_or(EnsError::MalformedName)?;
            let bytes = wire.get(at..end).ok_or(EnsError::MalformedName)?;
            let label = std::str::from_utf8(bytes)
                .map_err(|_| EnsError::MalformedName)?
                .to_string();
            labels.push(label);
            at = end;
        }
        if at != wire.len() {
            return Err(EnsError::MalformedName);
        }
        Ok(Self { labels })
    }

    /// EIP-137 namehash, folded right to left over the labels.
    pub fn node(&self) -> B256 {
        self.labels.iter().rev().fold(B256::ZERO, |parent, label| {
            keccak256(
                [parent.as_slice(), keccak256(label.as_bytes()).as_slice()].concat(),
            )
        })
    }

    /// The alphabet a handle-derived label may use: what X and GitHub reduce
    /// to after the substitution, and what Gmail's local part already is. A
    /// chain name must satisfy it too, which parsing a [`ChainName`] enforces.
    pub fn label_is_wellformed(label: &str) -> bool {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    /// The question this name asks of a gateway that answers under `domain`.
    pub fn query(&self, domain: &Domain) -> Result<Query, EnsError> {
        // The domain before anything else, so a name that is not ours is
        // refused as foreign whatever its labels look like. Label hygiene is
        // a statement about OUR names; a name under someone else's domain is
        // not owed it, and a caller must not be able to turn "foreign" into
        // "unreadable" — which is answered — by miscasing a label.
        let rest = self
            .labels
            .strip_suffix(domain.labels())
            .ok_or(EnsError::ForeignDomain)?;
        for label in rest {
            // ENS normalization never produces uppercase or whitespace.
            // Refusing here means the rest of this module never sees text a
            // wallet could not have sent.
            if label.is_empty()
                || label.bytes().any(|b| b.is_ascii_uppercase() || b <= b' ')
            {
                return Err(EnsError::UnnormalizedLabel(label.clone()));
            }
        }
        if rest.is_empty() {
            return Err(EnsError::EmptyName);
        }

        // Right to left. The rightmost label is either the platform, or a
        // chain label with the platform one further in.
        let (chain_label, marker_at) = match rest.last().map(String::as_str) {
            Some(last) if last.parse::<KnownPlatform>().is_ok() => (None, rest.len() - 1),
            Some(chain) => {
                if rest.len() < 2 {
                    return Err(EnsError::EmptyName);
                }
                (Some(chain.to_string()), rest.len() - 2)
            }
            None => return Err(EnsError::EmptyName),
        };

        let marker = rest[marker_at].as_str();
        let head = &rest[..marker_at];
        if head.is_empty() {
            return Err(EnsError::EmptyName);
        }

        // A platform label this build does not know is not a parse failure —
        // it is a name nobody can hold, which the caller learns as a null
        // answer.
        let platform: KnownPlatform = marker.parse().map_err(|_| EnsError::EmptyName)?;
        let handle = platform
            .handle_from_labels(head)
            .ok_or_else(|| EnsError::UnnormalizedLabel(head.join(".")))?;

        Ok(Query {
            subject: Subject::Handle { platform, handle },
            chain_label,
        })
    }
}

impl KnownPlatform {
    /// Invert the label transform for this platform.
    ///
    /// The forward direction maps the handles each platform issues, folded as
    /// the chain keys them, so this returns that byte form — no case, no
    /// padding, no leading at-sign to strip. A handle outside the platform's
    /// own format has no name.
    ///
    /// A method on the platform, so the `match` is exhaustive: a platform
    /// added to the type does not compile until its transform is written
    /// here. That guarantee is why the type exists.
    pub fn handle_from_labels(self, labels: &[String]) -> Option<String> {
        match self {
            // X and Telegram issue handles of `[a-z0-9_]` (folded), with no
            // hyphen, so `_` -> `-` is a bijection onto its image and reverses
            // by replacing every `-`. Telegram's rule: "A-z (case-insensitive),
            // 0-9 and underscores", 5-32 characters
            // (core.telegram.org/method/account.checkUsername).
            Self::X | Self::Telegram => {
                let [label] = labels else { return None };
                Name::label_is_wellformed(label).then(|| label.replace('-', "_"))
            }
            // GitHub's rules forbid a doubled hyphen and issue no underscores,
            // so the forward direction is the identity and so is this.
            Self::GitHub => {
                let [label] = labels else { return None };
                Name::label_is_wellformed(label).then(|| label.clone())
            }
            // Gmail split the local part at its dots; join them back and
            // append the domain the platform label implies. `+`, `-` and `_`
            // are refused rather than mapped: Gmail issues none of them, and a
            // mapping for characters no account can hold would be untested
            // code on a payment path.
            //
            // Two forms, told apart by the `_at` separator. Without it the
            // domain is the one the platform label implies; with it the
            // labels carry the domain themselves, which is what reaches a
            // Workspace address.
            //
            // `_at` cannot be mistaken for part of an address: an underscore
            // is not a legal ENS label byte, so the well-formedness check
            // below refuses it everywhere except as the separator this branch
            // consumes.
            //
            // An address carrying `_` or `+` still has NO name. Both are legal
            // in a Google address and neither can appear in a label, and no
            // substitution is available: unlike X, where `_` maps to `-`
            // because X forbids `-`, a Google address may hold both, so the
            // map would not be reversible — and an irreversible map on a
            // payment path is worse than no name.
            Self::Google => {
                if labels.is_empty() {
                    return None;
                }
                let all_wellformed = |part: &[String]| {
                    !part.is_empty() && part.iter().all(|l| Name::label_is_wellformed(l))
                };

                if let Some(at) = labels.iter().position(|l| l == "_at") {
                    let (local, domain) = (&labels[..at], &labels[at + 1..]);
                    return (all_wellformed(local) && all_wellformed(domain))
                        .then(|| format!("{}@{}", local.join("."), domain.join(".")));
                }

                // Gmail: the local part alone, and its alphabet is narrower
                // than a label's — Gmail issues no hyphen, so accepting one
                // here would invent an address nobody can hold.
                let local_ok = labels.iter().all(|l| {
                    !l.is_empty()
                        && l.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                });
                local_ok.then(|| format!("{}@gmail.com", labels.join(".")))
            }
        }
    }
}

/// What the gateway hands back for one query: the result, and how long it is
/// good for. Signed for one resolver and one request, then encoded as the
/// blob the resolver's callback decodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The ABI-encoded return data of the record call.
    pub result: Vec<u8>,
    /// Unix seconds after which the resolver refuses this reply.
    pub expires: u64,
}

impl Reply {
    /// What the gateway signs, byte-identical to
    /// `HandleResolver.makeSignatureHash` and to the `SignatureVerifier` of
    /// the ENS reference.
    ///
    /// `target` binds the reply to one resolver, `expires` to a window,
    /// `request` to the exact query and `result` to the exact answer. Drop any
    /// one and the reply becomes replayable somewhere it was never meant for.
    pub fn digest(&self, target: Address, request: &[u8]) -> B256 {
        let mut preimage = Vec::with_capacity(2 + 20 + 8 + 32 + 32);
        preimage.extend_from_slice(&[0x19, 0x00]);
        preimage.extend_from_slice(target.as_slice());
        preimage.extend_from_slice(&self.expires.to_be_bytes());
        preimage.extend_from_slice(keccak256(request).as_slice());
        preimage.extend_from_slice(keccak256(&self.result).as_slice());
        keccak256(preimage)
    }

    /// ABI-encode `(bytes result, uint64 expires, bytes signature)` — the blob
    /// the resolver's callback decodes.
    pub fn encode(&self, signature: &[u8]) -> Vec<u8> {
        (
            Bytes::copy_from_slice(&self.result),
            self.expires,
            Bytes::copy_from_slice(signature),
        )
            .abi_encode_params()
    }
}
