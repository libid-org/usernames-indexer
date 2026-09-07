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
//! chain outside the served set is refused rather than denied.
//!
//! Note what the range limit does and does not cost. `0x80000000 | chainId`
//! stops being injective at 2^31, so a coin type cannot be decoded back to one
//! chain id — but the forward direction is always well defined, and a chain
//! past 2^31 (the eden testnet's 3735928814 is one) is addressable like any
//! other. That is why [`coin_type_for`] is the only direction offered, and why
//! a gateway matches a query against its configured chains rather than
//! decoding. [`chain_ids_collide`] names the pair that cannot be served
//! together.

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

/// The chains a name may narrow to, by label — `alice.x.base.handles.link`.
///
/// A closed set that never overlaps the platform keys, and the ONE place a
/// chain's label is defined. The gateway answers for whatever chains the
/// store holds; this table only says what each is called in a name. A chain
/// absent here is still served under its coin type, and cannot be named by
/// label.
pub const KNOWN_CHAINS: [(u64, &str); 2] = [(3_735_928_814, "eden"), (8453, "base")];

/// The label a chain carries in a name, if it has one.
pub fn chain_label(chain_id: u64) -> Option<&'static str> {
    KNOWN_CHAINS
        .iter()
        .find(|(id, _)| *id == chain_id)
        .map(|(_, label)| *label)
}

/// ENSIP-11: an EVM chain's coin type is `0x80000000 | chainId`.
const EVM_COIN_TYPE_BIT: u64 = 0x8000_0000;

/// The coin type ENSIP-11 gives an EVM chain.
///
/// This direction is total and unambiguous for every chain id. It is the
/// REVERSE that is lossy: above 2^31 the bit is already set, the OR changes
/// nothing, and two chain ids land on one coin type. So a gateway matches a
/// query against the chains it was CONFIGURED with, using this function,
/// rather than computing a chain id back out of the coin type — which would
/// have to guess, and would guess wrong for any id past 2^31.
///
/// Chains that would collide are refused where both are known: at startup.
pub fn coin_type_for(chain_id: u64) -> U256 {
    U256::from(EVM_COIN_TYPE_BIT | chain_id)
}

/// Whether two chains cannot be told apart by their coin type.
pub fn chain_ids_collide(a: u64, b: u64) -> bool {
    a != b && coin_type_for(a) == coin_type_for(b)
}

/// Whether this coin type names an EVM chain at all.
///
/// A coin type without the ENSIP-11 bit belongs to another kind of chain
/// entirely — Bitcoin is 0 — and no libID binding is ever an address of that
/// kind. That is knowable without serving any chain, which is why such a query
/// earns a signed null rather than a refusal.
pub fn names_an_evm_chain(coin_type: U256) -> bool {
    let Ok(raw) = u64::try_from(coin_type) else {
        return false;
    };
    raw == COIN_TYPE_ETH || raw & EVM_COIN_TYPE_BIT != 0
}

/// Whether this coin type names Ethereum mainnet, which answers to two: the
/// legacy 60, and ENSIP-11's `0x80000001`.
pub fn is_mainnet_coin_type(coin_type: U256) -> bool {
    coin_type == U256::from(COIN_TYPE_ETH) || coin_type == coin_type_for(1)
}

/// Coin type 60 is Ethereum mainnet — a specific chain, not an unknown one.
const COIN_TYPE_ETH: u64 = 60;

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
        /// the configured chains is the gateway's job.
        coin_type: U256,
        /// Whether the caller used the legacy `addr(bytes32)` form, which
        /// returns a bare `address` rather than ENSIP-11's `bytes`.
        legacy: bool,
    },
    /// Any other record. Answered null rather than refused, so a client asking
    /// for `text()` or a future record type degrades instead of erroring.
    Other,
}

/// Split a DNS wire-format name into its labels.
///
/// `\x05alice\x01x\x07handles\x04link\x00` becomes `["alice", "x", "handles",
/// "link"]`. The root label terminates; trailing bytes after it are a
/// malformed name rather than something to ignore.
pub fn parse_dns_name(wire: &[u8]) -> Result<Vec<String>, EnsError> {
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
    Ok(labels)
}

/// EIP-137 namehash, folded right to left over the LABELS.
///
/// Over the list rather than over a joined name on purpose. A DNS wire label
/// may itself contain a dot — nothing in the format forbids it — so joining and
/// hashing the string would give `["a.b"]` and `["a", "b"]` one hash for two
/// different names. The wire format already said where the boundaries are;
/// rebuilding them out of a string throws that away.
pub fn namehash(labels: &[String]) -> B256 {
    labels.iter().rev().fold(B256::ZERO, |parent, label| {
        keccak256([parent.as_slice(), keccak256(label.as_bytes()).as_slice()].concat())
    })
}

/// The alphabet a handle-derived label may use: what X and GitHub reduce to
/// after the substitution, and what Gmail's local part already is.
///
/// Public because a CHAIN label must satisfy it too. [`parse_query`] does not
/// check it — a chain label is compared verbatim against the configured ones,
/// so an unusable label simply matches nothing — which is why the gateway
/// refuses such a label at STARTUP instead, reading the rule from here rather
/// than restating it.
pub fn label_is_wellformed(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Invert the label transform for one platform.
///
/// The forward direction is specified against what `HandleNormalizer`
/// produced, so this returns the handle in exactly the byte form the chain
/// keyed — no case, no padding, no leading at-sign to strip.
///
/// Takes [`KnownPlatform`] rather than a key, so the `match` below is exhaustive: a
/// platform added to that type does not compile until its transform is written
/// here. That guarantee is why the type exists.
fn labels_to_handle(platform: KnownPlatform, labels: &[String]) -> Option<String> {
    match platform {
        // X's alphabet is `[a-z0-9_]` and contains no hyphen, so `_` -> `-`
        // is a bijection onto its image and reverses by replacing every `-`.
        KnownPlatform::X => {
            let [label] = labels else { return None };
            label_is_wellformed(label).then(|| label.replace('-', "_"))
        }
        // GitHub's rules forbid a doubled hyphen and issue no underscores, so
        // the forward direction is the identity and so is this.
        KnownPlatform::GitHub => {
            let [label] = labels else { return None };
            label_is_wellformed(label).then(|| label.clone())
        }
        // Gmail split the local part at its dots; join them back and append
        // the domain the platform label implies. `+`, `-` and `_` are refused
        // rather than mapped: Gmail issues none of them, and a mapping for
        // characters no account can hold would be untested code on a payment
        // path.
        //
        // Two forms, told apart by the `_at` separator. Without it the domain
        // is the one the platform label implies; with it the labels carry the
        // domain themselves, which is what reaches a Workspace address.
        //
        // `_at` cannot be mistaken for part of an address: an underscore is not
        // a legal ENS label byte, so the well-formedness check below refuses it
        // everywhere except as the separator this branch consumes.
        //
        // An address carrying `_` or `+` still has NO name. Both are legal in a
        // Google address and neither can appear in a label, and no substitution
        // is available: unlike X, where `_` maps to `-` because X forbids `-`,
        // a Google address may hold both, so the map would not be reversible —
        // and an irreversible map on a payment path is worse than no name.
        KnownPlatform::Google => {
            if labels.is_empty() {
                return None;
            }
            let all_wellformed = |part: &[String]| {
                !part.is_empty() && part.iter().all(|l| label_is_wellformed(l))
            };

            if let Some(at) = labels.iter().position(|l| l == "_at") {
                let (local, domain) = (&labels[..at], &labels[at + 1..]);
                return (all_wellformed(local) && all_wellformed(domain))
                    .then(|| format!("{}@{}", local.join("."), domain.join(".")));
            }

            // Gmail: the local part alone, and its alphabet is narrower than a
            // label's — Gmail issues no hyphen, so accepting one here would
            // invent an address nobody can hold.
            let local_ok = labels.iter().all(|l| {
                !l.is_empty()
                    && l.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            });
            local_ok.then(|| format!("{}@gmail.com", labels.join(".")))
        }
    }
}

/// Parse the labels of a full name into the question it asks.
pub fn parse_query(labels: &[String]) -> Result<Query, EnsError> {
    // The domain before anything else, so a name that is not ours is refused
    // as foreign whatever its labels look like. Label hygiene is a statement
    // about OUR names; a name under someone else's domain is not owed it, and
    // a caller must not be able to turn "foreign" into "unreadable" — which
    // is answered — by miscasing a label.
    let rest = labels
        .strip_suffix(&DOMAIN.map(String::from))
        .ok_or(EnsError::ForeignDomain)?;
    for label in rest {
        // ENS normalization never produces uppercase or whitespace. Refusing
        // here means the rest of this module never sees text a wallet could
        // not have sent.
        if label.is_empty() || label.bytes().any(|b| b.is_ascii_uppercase() || b <= b' ')
        {
            return Err(EnsError::UnnormalizedLabel(label.clone()));
        }
    }
    if rest.is_empty() {
        return Err(EnsError::EmptyName);
    }

    // Right to left. The rightmost label is either the platform, or a chain
    // label with the platform one further in.
    let (chain_label, marker_at) = match rest.last().map(String::as_str) {
        Some(last) if KnownPlatform::from_key(last).is_some() => (None, rest.len() - 1),
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

    // A platform label this build does not know is not a parse failure — it
    // is a name nobody can hold, which the caller learns as a null answer.
    let platform = KnownPlatform::from_key(marker).ok_or(EnsError::EmptyName)?;
    let handle = labels_to_handle(platform, head)
        .ok_or_else(|| EnsError::UnnormalizedLabel(head.join(".")))?;

    Ok(Query {
        subject: Subject::Handle {
            platform: platform.into(),
            handle,
        },
        chain_label,
    })
}

/// Decode the `resolve(bytes name, bytes data)` call the resolver forwarded,
/// returning the DNS-encoded name and the record it wraps.
pub fn decode_resolve_calldata(call_data: &[u8]) -> Result<(Vec<u8>, Record), EnsError> {
    let (selector, body) = call_data.split_at_checked(4).ok_or(EnsError::NotResolve)?;
    if selector != RESOLVE_SELECTOR {
        return Err(EnsError::NotResolve);
    }
    let (name, inner) = <(Bytes, Bytes)>::abi_decode_params(body)
        .map_err(|_| EnsError::MalformedCallData)?;
    Ok((name.into(), decode_record(&inner)))
}

/// Which record the inner calldata asks for.
fn decode_record(inner: &[u8]) -> Record {
    let Some((selector, args)) = inner.split_at_checked(4) else {
        return Record::Other;
    };
    match selector {
        // addr(bytes32) — the node and nothing else, which is coin type 60 by
        // definition: Ethereum mainnet.
        s if s == ADDR_SELECTOR => match <(B256,)>::abi_decode_params(args) {
            Ok((node,)) => Record::Addr {
                node,
                coin_type: U256::from(COIN_TYPE_ETH),
                legacy: true,
            },
            Err(_) => Record::Other,
        },
        // addr(bytes32,uint256) — the coin type carried through exactly as sent.
        s if s == ADDR_COIN_SELECTOR => match <(B256, U256)>::abi_decode_params(args) {
            Ok((node, coin_type)) => Record::Addr {
                node,
                coin_type,
                legacy: false,
            },
            Err(_) => Record::Other,
        },
        _ => Record::Other,
    }
}

/// ABI-encode what an `addr` record returns, in the shape the caller asked in.
///
/// `None` is the null answer, and it is a real answer rather than an absence:
/// ENSIP-11's `addr` returns `bytes`, so null is the empty byte string, and
/// the legacy form returns the zero address.
pub fn encode_addr_result(legacy: bool, address: Option<Address>) -> Vec<u8> {
    if legacy {
        // `addr(bytes32)` returns `address`, whose null is the zero address.
        address.unwrap_or_default().abi_encode()
    } else {
        // ENSIP-11's `addr(bytes32,uint256)` returns `bytes`, whose null is the
        // empty byte string.
        Bytes::from(address.map(|a| a.to_vec()).unwrap_or_default()).abi_encode()
    }
}

/// What the gateway signs, byte-identical to `HandleResolver.makeSignatureHash`
/// and to the `SignatureVerifier` of the ENS reference.
///
/// `target` binds the answer to one resolver, `expires` to a window, `request`
/// to the exact query and `result` to the exact answer. Drop any one and the
/// answer becomes replayable somewhere it was never meant for.
pub fn signature_digest(
    target: Address,
    expires: u64,
    request: &[u8],
    result: &[u8],
) -> B256 {
    let mut preimage = Vec::with_capacity(2 + 20 + 8 + 32 + 32);
    preimage.extend_from_slice(&[0x19, 0x00]);
    preimage.extend_from_slice(target.as_slice());
    preimage.extend_from_slice(&expires.to_be_bytes());
    preimage.extend_from_slice(keccak256(request).as_slice());
    preimage.extend_from_slice(keccak256(result).as_slice());
    keccak256(preimage)
}

/// ABI-encode `(bytes result, uint64 expires, bytes signature)` — the blob the
/// resolver's callback decodes.
pub fn encode_response(result: &[u8], expires: u64, signature: &[u8]) -> Vec<u8> {
    (
        Bytes::copy_from_slice(result),
        expires,
        Bytes::copy_from_slice(signature),
    )
        .abi_encode_params()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `alice.x.handles.link`, DNS wire format.
    const ALICE_X: &[u8] = b"\x05alice\x01x\x07handles\x04link\x00";

    fn labels(name: &[u8]) -> Vec<String> {
        parse_dns_name(name).unwrap()
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
            assert_eq!(parse_dns_name(bad), Err(EnsError::MalformedName), "{bad:?}");
        }
    }

    // ─── The name shape ─────────────────────────────────────────────

    #[test]
    fn the_short_form_names_a_platform_and_no_chain() {
        let q = parse_query(&labels(ALICE_X)).unwrap();
        assert_eq!(handle_of(&q), "alice");
        assert_eq!(q.chain_label, None);
    }

    #[test]
    fn a_chain_label_is_carried_and_never_guessed() {
        let q = parse_query(&labels(b"\x05alice\x01x\x04base\x07handles\x04link\x00"))
            .unwrap();
        assert_eq!(handle_of(&q), "alice");
        assert_eq!(q.chain_label.as_deref(), Some("base"));
    }

    #[test]
    fn a_name_outside_the_domain_is_refused() {
        assert!(matches!(
            parse_query(&labels(b"\x05alice\x01x\x03eth\x00")),
            Err(EnsError::ForeignDomain)
        ));
    }

    #[test]
    fn the_domain_alone_names_nobody() {
        assert!(matches!(
            parse_query(&labels(b"\x07handles\x04link\x00")),
            Err(EnsError::EmptyName)
        ));
    }

    #[test]
    fn an_unnormalized_label_is_refused_before_anything_reads_it() {
        // ENS normalization never produces uppercase. A wallet could not have
        // sent this, so it is the caller's error rather than a null answer.
        assert!(matches!(
            parse_query(&["Alice".into(), "x".into(), "handles".into(), "link".into()]),
            Err(EnsError::UnnormalizedLabel(_))
        ));
    }

    // ─── The inverse transform ──────────────────────────────────────

    #[test]
    fn x_reverses_its_hyphen_substitution() {
        // `_` -> `-` is a bijection onto its image because X's alphabet holds
        // no hyphen, so this reverses exactly.
        let q = parse_query(&labels(b"\x04a--b\x01x\x07handles\x04link\x00")).unwrap();
        assert_eq!(handle_of(&q), "a__b");
    }

    #[test]
    fn github_labels_are_the_handle_unchanged() {
        let q =
            parse_query(&labels(b"\x05alice\x06github\x07handles\x04link\x00")).unwrap();
        assert_eq!(handle_of(&q), "alice");
    }

    #[test]
    fn gmail_joins_its_labels_and_appends_the_domain_the_platform_implies() {
        let q = parse_query(&labels(b"\x05alice\x01b\x06google\x07handles\x04link\x00"))
            .unwrap();
        assert_eq!(handle_of(&q), "alice.b@gmail.com");
    }

    #[test]
    fn gmail_refuses_characters_no_account_can_hold() {
        // Our email rules admit `-`, which Gmail does not issue. Refusing is
        // deliberate: a mapping for characters no account holds would be
        // untested code on a payment path.
        assert!(matches!(
            parse_query(&labels(b"\x06al-ice\x06google\x07handles\x04link\x00")),
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
        assert_eq!(parse_dns_name(&wire), Err(EnsError::MalformedName));
    }

    // ─── Google: the two address forms ──────────────────────────────

    #[test]
    fn a_gmail_address_keeps_its_implied_domain() {
        assert_eq!(
            labels_to_handle(KnownPlatform::Google, &["alice".into(), "smith".into()]),
            Some("alice.smith@gmail.com".into())
        );
    }

    /// The `_at` separator carries the domain, which is what reaches an
    /// address outside gmail.com — the case that had no name at all before.
    #[test]
    fn the_at_separator_carries_a_workspace_domain() {
        let labels = ["green", "baneling", "_at", "fuel", "sh"].map(String::from);
        assert_eq!(
            labels_to_handle(KnownPlatform::Google, &labels),
            Some("green.baneling@fuel.sh".into())
        );
    }

    /// A hyphen is legal in a label and in a domain, so it survives the round
    /// trip — unlike gmail's narrower alphabet.
    #[test]
    fn a_hyphenated_domain_survives() {
        let labels = ["alice", "_at", "my-company", "com"].map(String::from);
        assert_eq!(
            labels_to_handle(KnownPlatform::Google, &labels),
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
                labels_to_handle(KnownPlatform::Google, &labels),
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
        assert_eq!(labels_to_handle(KnownPlatform::Google, &labels), None);
    }

    // ─── Namehash ───────────────────────────────────────────────────

    /// EIP-137's own vectors, so the fold is pinned to the standard rather
    /// than to itself.
    #[test]
    fn namehash_matches_the_eip137_vectors() {
        let of =
            |name: &str| namehash(&name.split('.').map(String::from).collect::<Vec<_>>());
        assert_eq!(namehash(&[]), B256::ZERO);
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
        let one = namehash(&["a.b".to_string()]);
        let two = namehash(&["a".to_string(), "b".to_string()]);
        assert_ne!(one, two);
    }

    // ─── Coin types ─────────────────────────────────────────────────

    #[test]
    fn a_chain_gets_the_coin_type_ensip11_gives_it() {
        assert_eq!(coin_type_for(1), U256::from(0x8000_0001u64));
        assert_eq!(coin_type_for(8453), U256::from(0x8000_2105u64)); // Base
                                                                     // Mainnet answers to the legacy 60 as well.
        assert!(is_mainnet_coin_type(U256::from(60u64)));
        assert!(is_mainnet_coin_type(coin_type_for(1)));
        assert!(!is_mainnet_coin_type(coin_type_for(8453)));
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
        assert_eq!(coin_type_for(EDEN), U256::from(EDEN));
        assert_eq!(coin_type_for(EDEN), coin_type_for(TWIN));
        assert!(chain_ids_collide(EDEN, TWIN));

        // Everything else is unambiguous, eden included.
        assert!(!chain_ids_collide(EDEN, 8453));
        assert!(!chain_ids_collide(1, 8453));
        assert!(!chain_ids_collide(EDEN, EDEN));
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
        let node = namehash(&labels(ALICE_X));
        let call = resolve_call(ALICE_X, &addr_call(node, 0x8000_2105));
        let (name, record) = decode_resolve_calldata(&call).unwrap();
        assert_eq!(name, ALICE_X);
        assert_eq!(
            record,
            Record::Addr {
                node,
                coin_type: coin_type_for(8453),
                legacy: false
            }
        );
    }

    #[test]
    fn an_unknown_record_degrades_instead_of_erroring() {
        // `text(bytes32,string)` and anything else a future client asks for
        // must answer null, not fail: a refusal would surface in a wallet as
        // an error on a name that resolves fine for addresses.
        let call = resolve_call(ALICE_X, &[0xaa, 0xbb, 0xcc, 0xdd]);
        let (_, record) = decode_resolve_calldata(&call).unwrap();
        assert_eq!(record, Record::Other);
    }

    #[test]
    fn a_call_that_is_not_resolve_is_refused() {
        assert_eq!(
            decode_resolve_calldata(&[0xde, 0xad, 0xbe, 0xef]),
            Err(EnsError::NotResolve)
        );
        assert_eq!(decode_resolve_calldata(&[]), Err(EnsError::NotResolve));
    }

    // ─── The answer ─────────────────────────────────────────────────

    #[test]
    fn the_null_answer_is_an_answer_in_both_record_shapes() {
        // ENSIP-11 returns `bytes`: null is the empty byte string.
        assert_eq!(encode_addr_result(false, None).len(), 64);
        // The legacy form returns `address`: null is the zero address.
        assert_eq!(encode_addr_result(true, None), vec![0u8; 32]);
    }

    #[test]
    fn an_address_encodes_as_its_record_declares() {
        let who = Address::from([0x11u8; 20]);
        let legacy = encode_addr_result(true, Some(who));
        assert_eq!(&legacy[12..], who.as_slice());

        let ensip11 = encode_addr_result(false, Some(who));
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
            signature_digest(target, 1_800_000_000, &request, &result),
            "0xbec66310d9cf708669bddbdbb8f18bc4dc678c4742a3304a4972e274a73888a0"
                .parse::<B256>()
                .unwrap()
        );
    }

    #[test]
    fn the_response_encodes_the_three_fields_the_callback_decodes() {
        let out = encode_response(&[0xaa; 32], 1_800_000_000, &[0xbb; 65]);
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
        for (i, (id, label)) in KNOWN_CHAINS.iter().enumerate() {
            assert!(label_is_wellformed(label), "{label}");
            assert!(
                KnownPlatform::from_key(label).is_none(),
                "{label} is a platform"
            );
            assert_eq!(chain_label(*id), Some(*label));
            for (other_id, other_label) in &KNOWN_CHAINS[i + 1..] {
                assert_ne!(id, other_id);
                assert_ne!(label, other_label);
                assert!(!chain_ids_collide(*id, *other_id), "{id} and {other_id}");
            }
        }
        assert_eq!(chain_label(31341), None);
    }
}
