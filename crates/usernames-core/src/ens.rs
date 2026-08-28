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
//! <idNode as 64 hex> . _id [ . <chain> ] . handles . link
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
//! Note the range limit: `0x80000000 | chainId` is injective only below 2^31,
//! so a larger chain id cannot be named by ENSIP-11 at all. See
//! [`chain_id_is_addressable`].

use alloy::primitives::{
    keccak256,
    Address,
    B256,
    U256,
};

use crate::nodes::Platform;

/// The domain every name sits under, as labels.
pub const DOMAIN: [&str; 2] = ["handles", "link"];

/// The label marking an id-derived name. A leading underscore is the only
/// position ENS permits one, and no handle-derived label contains an
/// underscore at all, so this cannot collide with a platform label.
pub const ID_MARKER: &str = "_id";

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
    /// An account addressed by the storage key `IdentityNames` already uses.
    IdNode(B256),
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

/// The alphabet a handle-derived label may use: what X and GitHub reduce to
/// after the substitution, and what Gmail's local part already is.
fn label_is_wellformed(label: &str) -> bool {
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
fn labels_to_handle(key: &str, labels: &[String]) -> Option<String> {
    match key {
        // X's alphabet is `[a-z0-9_]` and contains no hyphen, so `_` -> `-`
        // is a bijection onto its image and reverses by replacing every `-`.
        "x" => {
            let [label] = labels else { return None };
            label_is_wellformed(label).then(|| label.replace('-', "_"))
        }
        // GitHub's rules forbid a doubled hyphen and issue no underscores, so
        // the forward direction is the identity and so is this.
        "github" => {
            let [label] = labels else { return None };
            label_is_wellformed(label).then(|| label.clone())
        }
        // Gmail split the local part at its dots; join them back and append
        // the domain the platform label implies. `+`, `-` and `_` are refused
        // rather than mapped: Gmail issues none of them, and a mapping for
        // characters no account can hold would be untested code on a payment
        // path.
        "google" => {
            if labels.is_empty() {
                return None;
            }
            let local_ok = labels.iter().all(|l| {
                !l.is_empty()
                    && l.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            });
            local_ok.then(|| format!("{}@gmail.com", labels.join(".")))
        }
        // A platform the registry knows but this function does not falls
        // through to `None`, which `parse_query` turns into an error and
        // `self_answer` into a SIGNED null — a real binding, authoritatively
        // denied. Nothing at runtime can tell that case from a genuinely
        // malformed label, so the guard is
        // `every_registered_platform_has_a_label_transform` rather than a
        // panic in a request handler.
        _ => None,
    }
}

/// Parse the labels of a full name into the question it asks.
pub fn parse_query(labels: &[String]) -> Result<Query, EnsError> {
    for label in labels {
        // ENS normalization never produces uppercase or whitespace. Refusing
        // here means the rest of this module never sees text a wallet could
        // not have sent.
        if label.is_empty() || label.bytes().any(|b| b.is_ascii_uppercase() || b <= b' ')
        {
            return Err(EnsError::UnnormalizedLabel(label.clone()));
        }
    }
    let rest = labels
        .strip_suffix(&DOMAIN.map(String::from))
        .ok_or(EnsError::ForeignDomain)?;
    if rest.is_empty() {
        return Err(EnsError::EmptyName);
    }

    // Right to left. The rightmost label is either the platform (or `_id`),
    // or a chain label with the platform one further in.
    let (chain_label, marker_at) = match rest.last().map(String::as_str) {
        Some(last) if last == ID_MARKER || Platform::from_key(last).is_some() => {
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

    if marker == ID_MARKER {
        // The id form carries the storage key itself, so the gateway answers
        // it from one lookup and never rebuilds an account id out of labels.
        // It has no platform label on purpose: `idNode` already derives from
        // the platform id, and a second statement of it could disagree.
        let [hex] = head else {
            return Err(EnsError::EmptyName);
        };
        let node = (hex.len() == 64)
            .then(|| hex.parse::<B256>().ok())
            .flatten()
            .ok_or_else(|| EnsError::UnnormalizedLabel(hex.clone()))?;
        return Ok(Query {
            subject: Subject::IdNode(node),
            chain_label,
        });
    }

    // A platform label this build does not know is not a parse failure — it
    // is a name nobody can hold, which the caller learns as a null answer.
    let platform = Platform::from_key(marker).ok_or(EnsError::EmptyName)?;
    let handle = labels_to_handle(marker, head)
        .ok_or_else(|| EnsError::UnnormalizedLabel(head.join(".")))?;

    Ok(Query {
        subject: Subject::Handle { platform, handle },
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
    let (name, inner) = decode_two_bytes(body).ok_or(EnsError::MalformedCallData)?;
    Ok((name, decode_record(&inner)))
}

/// `(bytes,bytes)` as ABI head-and-tail, by hand: two offsets, then two
/// length-prefixed payloads. Hand-rolled because pulling a full ABI decoder in
/// for one shape would be the larger dependency, and the shape is fixed.
fn decode_two_bytes(body: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let read_word = |at: usize| -> Option<usize> {
        // `checked_add`, because `at` comes from a word this same function
        // read: a hostile offset near `u64::MAX` overflows the range
        // expression and panics inside the request handler rather than
        // returning `None`.
        let w = body.get(at..at.checked_add(32)?)?;
        // A real offset or length fits far inside a usize; anything using the
        // high bytes is a hostile encoding, not a large value.
        if w[..24].iter().any(|b| *b != 0) {
            return None;
        }
        let mut n = [0u8; 8];
        n.copy_from_slice(&w[24..32]);
        usize::try_from(u64::from_be_bytes(n)).ok()
    };
    let read_bytes = |offset: usize| -> Option<Vec<u8>> {
        let len = read_word(offset)?;
        let start = offset.checked_add(32)?;
        let end = start.checked_add(len)?;
        body.get(start..end).map(<[u8]>::to_vec)
    };
    Some((read_bytes(read_word(0)?)?, read_bytes(read_word(32)?)?))
}

/// Which record the inner calldata asks for.
fn decode_record(inner: &[u8]) -> Record {
    let Some((selector, args)) = inner.split_at_checked(4) else {
        return Record::Other;
    };
    match (selector, args.len()) {
        // addr(bytes32) — the node and nothing else, which is coin type 60
        // by definition: Ethereum mainnet.
        (s, 32) if s == ADDR_SELECTOR => Record::Addr {
            coin_type: U256::from(COIN_TYPE_ETH),
            legacy: true,
        },
        // addr(bytes32,uint256) — carried through exactly as sent.
        (s, 64) if s == ADDR_COIN_SELECTOR => Record::Addr {
            coin_type: U256::from_be_slice(&args[32..64]),
            legacy: false,
        },
        _ => Record::Other,
    }
}

/// ABI-encode what the record returns.
///
/// `None` is the null answer, and it is a real answer rather than an absence:
/// ENSIP-11's `addr` returns `bytes`, so null is the empty byte string, and
/// the legacy form returns the zero address.
pub fn encode_addr_result(record: Record, address: Option<Address>) -> Vec<u8> {
    match record {
        Record::Addr { legacy: true, .. } => {
            let mut out = vec![0u8; 32];
            if let Some(address) = address {
                out[12..].copy_from_slice(address.as_slice());
            }
            out
        }
        // `bytes`: offset, length, then the padded payload.
        _ => {
            let mut out = vec![0u8; 64];
            out[31] = 0x20;
            if let Some(address) = address {
                out[63] = 20;
                let mut word = [0u8; 32];
                word[..20].copy_from_slice(address.as_slice());
                out.extend_from_slice(&word);
            }
            out
        }
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
    fn push_word(out: &mut Vec<u8>, n: u64) {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&w);
    }

    let mut out = Vec::new();
    let result_at = 3 * 32;
    let sig_at = result_at + 32 + result.len().div_ceil(32) * 32;
    push_word(&mut out, result_at as u64);
    push_word(&mut out, expires);
    push_word(&mut out, sig_at as u64);
    for payload in [result, signature] {
        push_word(&mut out, payload.len() as u64);
        out.extend_from_slice(payload);
        let pad = payload.len().div_ceil(32) * 32 - payload.len();
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out
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
            Subject::IdNode(n) => panic!("expected a handle, got id node {n}"),
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

    /// Adding a platform to the registry without a label transform here would
    /// answer every one of its names with a signed "nobody holds this". This
    /// is the only thing standing between that and production.
    #[test]
    fn every_registered_platform_has_a_label_transform() {
        for key in crate::nodes::platform_keys() {
            assert!(
                labels_to_handle(key, &["alice".to_string()]).is_some()
                    || labels_to_handle(key, &["alice".to_string(), "b".to_string()])
                        .is_some(),
                "platform {key:?} is in the registry but inverts no label; \
                 add its transform beside its normalization rules"
            );
        }
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

    /// The id-derived form parses — from labels. What it cannot do is arrive.
    ///
    /// The design writes it as `<idNode as 64 hex characters>._id.handles.link`,
    /// and 64 characters is one past what a DNS label may hold: RFC 1035 caps a
    /// label at 63 octets, and the two high bits of the length byte are
    /// reserved for compression, so 64 is not merely long but unencodable.
    /// Standard clients enforce it — ethers' `dnsEncode` refuses above 63 by
    /// default — which means no wallet can send this name in the first place.
    ///
    /// So the parse is written and tested against labels directly, and
    /// `a_sixty_four_character_label_cannot_be_encoded_at_all` records why the
    /// wire path stops short. Splitting the node across two labels, or encoding
    /// it in a shorter alphabet, would both work — but which one is a change to
    /// the name shape, and that belongs in the design rather than here.
    #[test]
    fn the_id_form_carries_the_storage_key_itself() {
        let node = "11".repeat(32);
        let q = parse_query(&[
            node.clone(),
            ID_MARKER.into(),
            "handles".into(),
            "link".into(),
        ])
        .unwrap();
        match q.subject {
            Subject::IdNode(n) => assert_eq!(n, node.parse::<B256>().unwrap()),
            other => panic!("expected an id node, got {other:?}"),
        }
        // No platform label: `idNode` derives from the platform id already,
        // and a second statement of it could disagree with the first.
        assert_eq!(q.chain_label, None);
    }

    #[test]
    fn a_sixty_four_character_label_cannot_be_encoded_at_all() {
        let node = "11".repeat(32);
        let mut wire = vec![node.len() as u8]; // 64
        wire.extend_from_slice(node.as_bytes());
        wire.extend_from_slice(b"\x03_id\x07handles\x04link\x00");
        assert_eq!(parse_dns_name(&wire), Err(EnsError::MalformedName));
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

    fn addr_call(coin: u64) -> Vec<u8> {
        let mut inner = ADDR_COIN_SELECTOR.to_vec();
        inner.extend_from_slice(&[0u8; 31]);
        inner.push(1);
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
        let call = resolve_call(ALICE_X, &addr_call(0x8000_2105));
        let (name, record) = decode_resolve_calldata(&call).unwrap();
        assert_eq!(name, ALICE_X);
        assert_eq!(
            record,
            Record::Addr {
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
        let ensip11 = Record::Addr {
            coin_type: coin_type_for(1),
            legacy: false,
        };
        // ENSIP-11 returns `bytes`: null is the empty byte string.
        assert_eq!(encode_addr_result(ensip11, None).len(), 64);
        // The legacy form returns `address`: null is the zero address.
        let legacy = Record::Addr {
            coin_type: coin_type_for(1),
            legacy: true,
        };
        assert_eq!(encode_addr_result(legacy, None), vec![0u8; 32]);
    }

    #[test]
    fn an_address_encodes_as_its_record_declares() {
        let who = Address::from([0x11u8; 20]);
        let legacy = encode_addr_result(
            Record::Addr {
                coin_type: coin_type_for(1),
                legacy: true,
            },
            Some(who),
        );
        assert_eq!(&legacy[12..], who.as_slice());

        let ensip11 = encode_addr_result(
            Record::Addr {
                coin_type: coin_type_for(1),
                legacy: false,
            },
            Some(who),
        );
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
        let request = resolve_call(ALICE_X, &addr_call(0x8000_2105));
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
