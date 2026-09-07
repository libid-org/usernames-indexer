//! The ENS half: turning a name under `handles.link` back into a question the
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
//! variable number of labels. Platform names and chain names are closed sets
//! that never overlap, which is what keeps it unambiguous.
//!
//! # Which chain
//!
//! A coin type names one chain, and mainnet is one of them rather than a
//! default: bare `addr(node)` is coin type 60, which is Ethereum mainnet
//! specifically. Whether an answer is owed for it is the gateway's decision
//! rather than this module's — see `usernames-api`'s `ens` module, where a
//! chain the store does not hold is refused rather than denied.
//!
//! Note what the range limit does and does not cost. `0x80000000 | chainId`
//! stops being injective at 2^31, so a coin type cannot be decoded back to one
//! chain id — but the forward direction is always well defined, and a chain
//! past 2^31 (the eden testnet's 3735928814 is one) is addressable like any
//! other. That is why [`CoinType::of_chain`] is the only direction offered,
//! and why a gateway matches a query against the chains its store holds
//! rather than decoding. [`CoinType::shared_by`] names the pair that cannot
//! be served together.

use std::fmt;

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

use crate::nodes::{
    KnownPlatform,
    Platform,
};

/// The domain every name sits under, as labels.
pub const DOMAIN: [&str; 2] = ["handles", "link"];

/// A chain a name may narrow to, by label — `alice.x.base.handles.link`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownChain {
    /// The chain id, as the chain reports it.
    pub id: u64,
    /// What the chain is called in a name.
    pub label: &'static str,
}

/// The labels the gateway ANSWERS for, and the one place a chain's label is
/// defined: the parser carries whatever label it finds, and a name naming a
/// label not here is answered as a name nobody holds. Never overlaps the
/// platform keys, which is what lets the parse tell the two apart. The
/// gateway serves whatever chains the store holds; a chain absent here is
/// still served under its coin type, and cannot be named by label. Adding
/// one is a release, not a deploy.
pub const KNOWN_CHAINS: [KnownChain; 2] = [
    KnownChain {
        id: 3_735_928_814,
        label: "eden",
    },
    KnownChain {
        id: 8453,
        label: "base",
    },
];

impl KnownChain {
    /// The label a chain carries in a name, if it has one.
    pub fn label_of(chain_id: u64) -> Option<&'static str> {
        KNOWN_CHAINS
            .iter()
            .find(|chain| chain.id == chain_id)
            .map(|chain| chain.label)
    }
}

/// ENSIP-11: an EVM chain's coin type is `0x80000000 | chainId`.
const EVM_COIN_TYPE_BIT: u64 = 0x8000_0000;

/// Coin type 60 is Ethereum mainnet — a specific chain, not an unknown one.
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

    /// The raw value, as it travels in `addr(bytes32,uint256)`.
    pub fn raw(self) -> U256 {
        self.0
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

    /// Whether this coin type names Ethereum mainnet, which answers to two:
    /// the legacy 60, and ENSIP-11's `0x80000001`.
    pub fn is_mainnet(self) -> bool {
        self.0 == U256::from(COIN_TYPE_ETH) || self == Self::of_chain(1)
    }

    /// Whether this coin type names the given chain — forward, never decoded.
    pub fn names_chain(self, chain_id: u64) -> bool {
        Self::of_chain(chain_id) == self || (chain_id == 1 && self.is_mainnet())
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
const ADDR_COIN_SELECTOR: [u8; 4] = [0xf1, 0xcb, 0x7e, 0x06];

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
    #[error("name is not under handles.link")]
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
        platform: Platform,
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
    pub fn decode(inner: &[u8]) -> Self {
        let Some((selector, args)) = inner.split_at_checked(4) else {
            return Self::Other;
        };
        match selector {
            // addr(bytes32) — the node and nothing else, which is coin type 60
            // by definition: Ethereum mainnet.
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
    /// Split a DNS wire-format name into its labels.
    ///
    /// `\x05alice\x01x\x07handles\x04link\x00` becomes `["alice", "x",
    /// "handles", "link"]`. The root label terminates; trailing bytes after it
    /// are a malformed name rather than something to ignore.
    pub fn parse(wire: &[u8]) -> Result<Self, EnsError> {
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

    /// A name from labels already split — what a test or a builder holds.
    pub fn from_labels(labels: Vec<String>) -> Self {
        Self { labels }
    }

    /// The labels, leftmost first.
    pub fn labels(&self) -> &[String] {
        &self.labels
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
    /// chain label must satisfy it too, which the `KNOWN_CHAINS` test pins.
    pub fn label_is_wellformed(label: &str) -> bool {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    /// The question this name asks.
    pub fn query(&self) -> Result<Query, EnsError> {
        // The domain before anything else, so a name that is not ours is
        // refused as foreign whatever its labels look like. Label hygiene is
        // a statement about OUR names; a name under someone else's domain is
        // not owed it, and a caller must not be able to turn "foreign" into
        // "unreadable" — which is answered — by miscasing a label.
        let rest = self
            .labels
            .strip_suffix(&DOMAIN.map(String::from))
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
            Some(last) if KnownPlatform::from_key(last).is_some() => {
                (None, rest.len() - 1)
            }
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
        let platform = KnownPlatform::from_key(marker).ok_or(EnsError::EmptyName)?;
        let handle = platform
            .handle_from_labels(head)
            .ok_or_else(|| EnsError::UnnormalizedLabel(head.join(".")))?;

        Ok(Query {
            subject: Subject::Handle {
                platform: platform.into(),
                handle,
            },
            chain_label,
        })
    }
}

impl KnownPlatform {
    /// Invert the label transform for this platform.
    ///
    /// The forward direction is specified against what `HandleNormalizer`
    /// produced, so this returns the handle in exactly the byte form the chain
    /// keyed — no case, no padding, no leading at-sign to strip.
    ///
    /// A method on the platform, so the `match` is exhaustive: a platform
    /// added to the type does not compile until its transform is written
    /// here. That guarantee is why the type exists.
    fn handle_from_labels(self, labels: &[String]) -> Option<String> {
        match self {
            // X's alphabet is `[a-z0-9_]` and contains no hyphen, so `_` ->
            // `-` is a bijection onto its image and reverses by replacing
            // every `-`.
            Self::X => {
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
    /// A reply good for `ttl_secs` from now.
    pub fn good_for(result: Vec<u8>, ttl_secs: u64) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        Self {
            result,
            expires: now.saturating_add(ttl_secs),
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// `alice.x.handles.link`, DNS wire format.
    const ALICE_X: &[u8] = b"\x05alice\x01x\x07handles\x04link\x00";

    fn labels(name: &[u8]) -> Vec<String> {
        Name::parse(name).unwrap().labels().to_vec()
    }

    fn handle_of(q: &Query) -> String {
        match &q.subject {
            Subject::Handle { handle, .. } => handle.clone(),
        }
    }

    // ─── The wire name ──────────────────────────────────────────────

    #[test]
    fn a_wire_name_splits_into_its_labels() {
        assert_eq!(labels(ALICE_X), ["alice", "x", "handles", "link"]);
    }

    #[test]
    fn a_truncated_or_overrun_name_is_refused() {
        for bad in [
            &b""[..],                       // no root label
            &b"\x05alic\x00"[..],           // length overruns the buffer
            &b"\x05alice\x01x"[..],         // unterminated
            &b"\x05alice\x00\x01x\x00"[..], // bytes after the root label
            &b"\x40aaaaaaaa\x00"[..],       // length above the 63-byte ceiling
        ] {
            assert_eq!(Name::parse(bad), Err(EnsError::MalformedName), "{bad:?}");
        }
    }

    // ─── The name shape ─────────────────────────────────────────────

    #[test]
    fn the_short_form_names_a_platform_and_no_chain() {
        let q = Name::from_labels(labels(ALICE_X)).query().unwrap();
        assert_eq!(handle_of(&q), "alice");
        assert_eq!(q.chain_label, None);
    }

    #[test]
    fn a_chain_label_is_carried_and_never_guessed() {
        let q =
            Name::from_labels(labels(b"\x05alice\x01x\x04base\x07handles\x04link\x00"))
                .query()
                .unwrap();
        assert_eq!(handle_of(&q), "alice");
        assert_eq!(q.chain_label.as_deref(), Some("base"));
    }

    #[test]
    fn a_name_outside_the_domain_is_refused() {
        assert!(matches!(
            Name::from_labels(labels(b"\x05alice\x01x\x03eth\x00")).query(),
            Err(EnsError::ForeignDomain)
        ));
    }

    #[test]
    fn the_domain_alone_names_nobody() {
        assert!(matches!(
            Name::from_labels(labels(b"\x07handles\x04link\x00")).query(),
            Err(EnsError::EmptyName)
        ));
    }

    #[test]
    fn an_unnormalized_label_is_refused_before_anything_reads_it() {
        // ENS normalization never produces uppercase. A wallet could not have
        // sent this, so it is the caller's error rather than a null answer.
        assert!(matches!(
            Name::from_labels(vec![
                "Alice".into(),
                "x".into(),
                "handles".into(),
                "link".into()
            ])
            .query(),
            Err(EnsError::UnnormalizedLabel(_))
        ));
    }

    // ─── The inverse transform ──────────────────────────────────────

    #[test]
    fn x_reverses_its_hyphen_substitution() {
        // `_` -> `-` is a bijection onto its image because X's alphabet holds
        // no hyphen, so this reverses exactly.
        let q = Name::from_labels(labels(b"\x04a--b\x01x\x07handles\x04link\x00"))
            .query()
            .unwrap();
        assert_eq!(handle_of(&q), "a__b");
    }

    #[test]
    fn github_labels_are_the_handle_unchanged() {
        let q = Name::from_labels(labels(b"\x05alice\x06github\x07handles\x04link\x00"))
            .query()
            .unwrap();
        assert_eq!(handle_of(&q), "alice");
    }

    #[test]
    fn gmail_joins_its_labels_and_appends_the_domain_the_platform_implies() {
        let q =
            Name::from_labels(labels(b"\x05alice\x01b\x06google\x07handles\x04link\x00"))
                .query()
                .unwrap();
        assert_eq!(handle_of(&q), "alice.b@gmail.com");
    }

    #[test]
    fn gmail_refuses_characters_no_account_can_hold() {
        // Our email rules admit `-`, which Gmail does not issue. Refusing is
        // deliberate: a mapping for characters no account holds would be
        // untested code on a payment path.
        assert!(matches!(
            Name::from_labels(labels(b"\x06al-ice\x06google\x07handles\x04link\x00"))
                .query(),
            Err(EnsError::UnnormalizedLabel(_))
        ));
    }

    /// A label may hold 63 octets. The ceiling is the reason the design's
    /// id-derived name — the storage key as 64 hex characters — was dropped
    /// rather than implemented: it is one past what a name can carry, so no
    /// client could ever send it. Standard encoders enforce this (ethers'
    /// `dnsEncode` refuses above 63), which makes it unencodable rather than
    /// merely long.
    #[test]
    fn a_label_past_sixty_three_octets_cannot_be_encoded() {
        let long = "1".repeat(64);
        let mut wire = vec![long.len() as u8];
        wire.extend_from_slice(long.as_bytes());
        wire.extend_from_slice(b"\x07handles\x04link\x00");
        assert_eq!(Name::parse(&wire), Err(EnsError::MalformedName));
    }

    // ─── Google: the two address forms ──────────────────────────────

    #[test]
    fn a_gmail_address_keeps_its_implied_domain() {
        assert_eq!(
            KnownPlatform::Google.handle_from_labels(&["alice".into(), "smith".into()]),
            Some("alice.smith@gmail.com".into())
        );
    }

    /// The `_at` separator carries the domain, which is what reaches an
    /// address outside gmail.com — the case that had no name at all before.
    #[test]
    fn the_at_separator_carries_a_workspace_domain() {
        let labels = ["green", "baneling", "_at", "fuel", "sh"].map(String::from);
        assert_eq!(
            KnownPlatform::Google.handle_from_labels(&labels),
            Some("green.baneling@fuel.sh".into())
        );
    }

    /// A hyphen is legal in a label and in a domain, so it survives the round
    /// trip — unlike gmail's narrower alphabet.
    #[test]
    fn a_hyphenated_domain_survives() {
        let labels = ["alice", "_at", "my-company", "com"].map(String::from);
        assert_eq!(
            KnownPlatform::Google.handle_from_labels(&labels),
            Some("alice@my-company.com".into())
        );
    }

    /// Both sides must exist: `_at` at either edge names no address.
    #[test]
    fn a_separator_at_the_edge_is_refused() {
        for labels in [
            vec!["_at".to_string(), "fuel".into()],
            vec!["alice".into(), "_at".into()],
        ] {
            assert_eq!(
                KnownPlatform::Google.handle_from_labels(&labels),
                None,
                "{labels:?}"
            );
        }
    }

    /// A second `_at` cannot be smuggled in as part of the address: an
    /// underscore is not a legal label byte, so the domain half refuses it and
    /// the form stays unambiguous.
    #[test]
    fn a_second_separator_is_refused() {
        let labels = ["a", "_at", "b", "_at", "c"].map(String::from);
        assert_eq!(KnownPlatform::Google.handle_from_labels(&labels), None);
    }

    // ─── Namehash ───────────────────────────────────────────────────

    /// EIP-137's own vectors, so the fold is pinned to the standard rather
    /// than to itself.
    #[test]
    fn namehash_matches_the_eip137_vectors() {
        let of = |name: &str| {
            Name::from_labels(name.split('.').map(String::from).collect()).node()
        };
        assert_eq!(Name::from_labels(vec![]).node(), B256::ZERO);
        assert_eq!(
            of("eth").to_string(),
            "0x93cdeb708b7545dc668eb9280176169d1c33cfd8ed6f04690a0bcc88a93fc4ae"
        );
        assert_eq!(
            of("foo.eth").to_string(),
            "0xde9b09fd7c5f901e23a3f19fecc54828e9c848539801e86591bd9801b019f84f"
        );
        assert_eq!(
            of("alice.x.handles.link").to_string(),
            "0x21a53adadf8699366708518cccb5ff27860eb2559be8d4c552dede0b5eb81f86"
        );
    }

    /// The reason this folds over labels and not over a joined string. A DNS
    /// label may hold a dot, so `["a.b"]` and `["a", "b"]` are two names — and
    /// a namehash taken over `labels.join(".")` would give them one node, and
    /// answer either with the other's address.
    #[test]
    fn a_label_holding_a_dot_is_not_two_labels() {
        let one = Name::from_labels(vec!["a.b".to_string()]).node();
        let two = Name::from_labels(vec!["a".to_string(), "b".to_string()]).node();
        assert_ne!(one, two);
    }

    // ─── Coin types ─────────────────────────────────────────────────

    #[test]
    fn a_chain_gets_the_coin_type_ensip11_gives_it() {
        assert_eq!(CoinType::of_chain(1).raw(), U256::from(0x8000_0001u64));
        assert_eq!(CoinType::of_chain(8453).raw(), U256::from(0x8000_2105u64)); // Base
                                                                                // Mainnet answers to the legacy 60 as well.
        assert!(CoinType::from(U256::from(60u64)).is_mainnet());
        assert!(CoinType::of_chain(1).is_mainnet());
        assert!(!CoinType::of_chain(8453).is_mainnet());
    }

    /// The eden testnet, and why matching forwards is what makes it work.
    ///
    /// 3735928814 is 0xDEADBFEE, so bit 31 is already set and the OR changes
    /// nothing. Going FORWARD that is fine — the coin type is well defined and
    /// a wallet on eden sends exactly it. What is lost is the reverse: 0xDEADBFEE
    /// could equally have come from 1588445166. So a gateway compares coin
    /// types instead of decoding them, and refuses only a configuration that
    /// serves BOTH of the two colliding chains.
    #[test]
    fn a_chain_past_thirty_one_bits_is_addressable_unless_its_twin_is_served() {
        const EDEN: u64 = 3_735_928_814; // 0xDEADBFEE
        const TWIN: u64 = 1_588_445_166; // 0x5EADBFEE

        // The coin type a wallet on eden sends, and it names eden exactly.
        assert_eq!(CoinType::of_chain(EDEN).raw(), U256::from(EDEN));
        assert_eq!(CoinType::of_chain(EDEN), CoinType::of_chain(TWIN));
        assert!(CoinType::shared_by(EDEN, TWIN));

        // Everything else is unambiguous, eden included.
        assert!(!CoinType::shared_by(EDEN, 8453));
        assert!(!CoinType::shared_by(1, 8453));
        assert!(!CoinType::shared_by(EDEN, EDEN));
    }

    // ─── The call ───────────────────────────────────────────────────

    /// Takes the node rather than inventing one: a real request carries the
    /// namehash of the name beside it, and a fixture that does not is a
    /// fixture no client would send.
    fn addr_call(node: B256, coin: u64) -> Vec<u8> {
        let mut inner = ADDR_COIN_SELECTOR.to_vec();
        inner.extend_from_slice(node.as_slice());
        inner.extend_from_slice(&U256::from(coin).to_be_bytes::<32>());
        inner
    }

    fn resolve_call(name: &[u8], inner: &[u8]) -> Vec<u8> {
        let mut out = RESOLVE_SELECTOR.to_vec();
        let mut word = |n: u64| {
            let mut w = [0u8; 32];
            w[24..].copy_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&w);
        };
        word(0x40);
        word(0x40 + 32 + name.len().div_ceil(32) as u64 * 32);
        for payload in [name, inner] {
            let mut w = [0u8; 32];
            w[24..].copy_from_slice(&(payload.len() as u64).to_be_bytes());
            out.extend_from_slice(&w);
            out.extend_from_slice(payload);
            let pad = payload.len().div_ceil(32) * 32 - payload.len();
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out
    }

    #[test]
    fn the_forwarded_call_decodes_to_a_name_and_a_record() {
        let node = Name::from_labels(labels(ALICE_X)).node();
        let call = resolve_call(ALICE_X, &addr_call(node, 0x8000_2105));
        let ResolveCall { name, record } = ResolveCall::decode(&call).unwrap();
        assert_eq!(name, ALICE_X);
        assert_eq!(
            record,
            Record::Addr {
                node,
                coin_type: CoinType::of_chain(8453),
                shape: AddrShape::Multichain
            }
        );
    }

    #[test]
    fn an_unknown_record_degrades_instead_of_erroring() {
        // `text(bytes32,string)` and anything else a future client asks for
        // must answer null, not fail: a refusal would surface in a wallet as
        // an error on a name that resolves fine for addresses.
        let call = resolve_call(ALICE_X, &[0xaa, 0xbb, 0xcc, 0xdd]);
        let ResolveCall { record, .. } = ResolveCall::decode(&call).unwrap();
        assert_eq!(record, Record::Other);
    }

    #[test]
    fn a_call_that_is_not_resolve_is_refused() {
        assert_eq!(
            ResolveCall::decode(&[0xde, 0xad, 0xbe, 0xef]),
            Err(EnsError::NotResolve)
        );
        assert_eq!(ResolveCall::decode(&[]), Err(EnsError::NotResolve));
    }

    // ─── The answer ─────────────────────────────────────────────────

    #[test]
    fn the_null_answer_is_an_answer_in_both_record_shapes() {
        // ENSIP-11 returns `bytes`: null is the empty byte string.
        assert_eq!(AddrShape::Multichain.encode(None).len(), 64);
        // The legacy form returns `address`: null is the zero address.
        assert_eq!(AddrShape::Legacy.encode(None), vec![0u8; 32]);
    }

    #[test]
    fn an_address_encodes_as_its_record_declares() {
        let who = Address::from([0x11u8; 20]);
        let legacy = AddrShape::Legacy.encode(Some(who));
        assert_eq!(&legacy[12..], who.as_slice());

        let ensip11 = AddrShape::Multichain.encode(Some(who));
        assert_eq!(ensip11.len(), 96);
        assert_eq!(ensip11[31], 0x20); // offset
        assert_eq!(ensip11[63], 20); // length
        assert_eq!(&ensip11[64..84], who.as_slice());
    }

    /// The digest this gateway signs must equal the one `HandleResolver`
    /// recomputes, or every answer is rejected on chain.
    ///
    /// The expected value is not computed here — it comes from `cast` over the
    /// same inputs the Solidity test uses, so this pins Rust against the EVM
    /// rather than against itself.
    #[test]
    fn the_digest_matches_the_solidity_resolver() {
        // The node is the vector's input, not a meaningful namehash: these are
        // the exact bytes `cast` and the Solidity test were given, and the
        // digest is only a digest of them.
        let request =
            resolve_call(ALICE_X, &addr_call(B256::with_last_byte(1), 0x8000_2105));
        let mut result = vec![0u8; 32];
        result[30] = 0xbe;
        result[31] = 0xef;

        let target = Address::from([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xaa,
        ]);
        assert_eq!(
            Reply {
                result: result.clone(),
                expires: 1_800_000_000
            }
            .digest(target, &request),
            "0xbec66310d9cf708669bddbdbb8f18bc4dc678c4742a3304a4972e274a73888a0"
                .parse::<B256>()
                .unwrap()
        );
    }

    #[test]
    fn the_response_encodes_the_three_fields_the_callback_decodes() {
        let out = Reply {
            result: vec![0xaa; 32],
            expires: 1_800_000_000,
        }
        .encode(&[0xbb; 65]);
        // head: offset, expires, offset
        assert_eq!(out[31], 0x60);
        assert_eq!(&out[56..64], &1_800_000_000u64.to_be_bytes());
        // result length, then signature length at its own offset
        assert_eq!(out[3 * 32 + 31], 32);
        let sig_at = 3 * 32 + 32 + 32;
        assert_eq!(out[sig_at + 31], 65);
        assert_eq!(out.len(), sig_at + 32 + 96);
    }
}

#[cfg(test)]
mod known_chains {
    use super::*;

    /// The table is the grammar's closed set: every label well-formed, no two
    /// chains sharing a label or a coin type, and no label a platform could be
    /// mistaken for — the parse decides "platform or chain" by the platform
    /// keys, so an overlap would make one name mean two things.
    #[test]
    fn chain_labels_form_a_closed_set_apart_from_the_platforms() {
        for (i, KnownChain { id, label }) in KNOWN_CHAINS.iter().enumerate() {
            assert!(Name::label_is_wellformed(label), "{label}");
            assert!(
                KnownPlatform::from_key(label).is_none(),
                "{label} is a platform"
            );
            assert_eq!(KnownChain::label_of(*id), Some(*label));
            for KnownChain {
                id: other_id,
                label: other_label,
            } in &KNOWN_CHAINS[i + 1..]
            {
                assert_ne!(id, other_id);
                assert_ne!(label, other_label);
                assert!(!CoinType::shared_by(*id, *other_id), "{id} and {other_id}");
            }
        }
        assert_eq!(KnownChain::label_of(31341), None);
    }
}
