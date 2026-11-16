//! The ENS gateway: resolution of handles.link names.

use alloy::primitives::{
    Address,
    B256,
    U256,
};
use usernames_core::nodes::KnownPlatform;

use usernames_core::ens::*;

/// `alice.x.handles.link`, DNS wire format.
const ALICE_X: &[u8] = b"\x05alice\x01x\x07handles\x04link\x00";

/// The name `labels` spell, parsed from the DNS wire format a wallet
/// sends.
fn named(labels: &[&str]) -> Name {
    let mut wire = Vec::new();
    for label in labels {
        wire.push(u8::try_from(label.len()).unwrap());
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);
    Name::decode(&wire).unwrap()
}

/// The domain the mainnet deployment answers under.
fn handles_link() -> Domain {
    DEFAULT_DOMAIN.parse().unwrap()
}

/// The domain the testnet deployment answers under.
fn testnet_handles_link() -> Domain {
    "testnet.handles.link".parse().unwrap()
}

/// The handle a query asks about.
trait QueryHandle {
    /// The handle, as the query names it.
    fn handle(&self) -> String;
}

impl QueryHandle for Query {
    fn handle(&self) -> String {
        match &self.subject {
            Subject::Handle { handle, .. } => handle.clone(),
        }
    }
}

// ─── The wire name ──────────────────────────────────────────────

#[test]
fn a_truncated_or_overrun_name_is_refused() {
    for bad in [
        &b""[..],                       // no root label
        &b"\x05alic\x00"[..],           // length overruns the buffer
        &b"\x05alice\x01x"[..],         // unterminated
        &b"\x05alice\x00\x01x\x00"[..], // bytes after the root label
        &b"\x40aaaaaaaa\x00"[..],       // length above the 63-byte ceiling
    ] {
        assert_eq!(Name::decode(bad), Err(EnsError::MalformedName), "{bad:?}");
    }
}

// ─── The name shape ─────────────────────────────────────────────

#[test]
fn the_short_form_names_a_platform_and_no_chain() {
    let q = Name::decode(ALICE_X)
        .unwrap()
        .query(&handles_link())
        .unwrap();
    assert_eq!(q.handle(), "alice");
    assert_eq!(q.chain_label, None);
}

#[test]
fn a_chain_label_is_carried_and_never_guessed() {
    let q = Name::decode(b"\x05alice\x01x\x04base\x07handles\x04link\x00")
        .unwrap()
        .query(&handles_link())
        .unwrap();
    assert_eq!(q.handle(), "alice");
    assert_eq!(q.chain_label.as_deref(), Some("base"));
}

#[test]
fn a_name_outside_the_domain_is_refused() {
    assert!(matches!(
        Name::decode(b"\x05alice\x01x\x03eth\x00")
            .unwrap()
            .query(&handles_link()),
        Err(EnsError::ForeignDomain)
    ));
}

#[test]
fn a_subname_domain_takes_its_names_and_leaves_the_parents() {
    let alice = b"\x05alice\x01x\x07testnet\x07handles\x04link\x00";
    let q = Name::decode(alice)
        .unwrap()
        .query(&testnet_handles_link())
        .unwrap();
    assert_eq!(q.handle(), "alice");
    assert_eq!(q.chain_label, None);

    // The parent's own names are not this gateway's to speak about.
    assert!(matches!(
        Name::decode(ALICE_X)
            .unwrap()
            .query(&testnet_handles_link()),
        Err(EnsError::ForeignDomain)
    ));
}

#[test]
fn a_chain_label_sits_before_a_subname_domain_too() {
    let alice = b"\x05alice\x01x\x07sepolia\x07testnet\x07handles\x04link\x00";
    let q = Name::decode(alice)
        .unwrap()
        .query(&testnet_handles_link())
        .unwrap();
    assert_eq!(q.handle(), "alice");
    assert_eq!(q.chain_label.as_deref(), Some("sepolia"));
}

#[test]
fn a_domain_reads_and_writes_as_dotted_labels() {
    let domain = testnet_handles_link();
    assert_eq!(domain.labels(), ["testnet", "handles", "link"]);
    assert_eq!(domain.to_string(), "testnet.handles.link");
    for bad in ["Handles.link", "handles..link", ""] {
        assert!(
            matches!(bad.parse::<Domain>(), Err(EnsError::UnnormalizedLabel(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn the_domain_alone_names_nobody() {
    assert!(matches!(
        Name::decode(b"\x07handles\x04link\x00")
            .unwrap()
            .query(&handles_link()),
        Err(EnsError::EmptyName)
    ));
}

#[test]
fn an_unnormalized_label_is_refused_before_anything_reads_it() {
    // ENS normalization never produces uppercase. A wallet could not have
    // sent this, so it is the caller's error rather than a null answer.
    assert!(matches!(
        named(&["Alice", "x", "handles", "link"]).query(&handles_link()),
        Err(EnsError::UnnormalizedLabel(_))
    ));
}

// ─── The inverse transform ──────────────────────────────────────

#[test]
fn x_reverses_its_hyphen_substitution() {
    // `_` -> `-` is a bijection onto its image because X's alphabet holds
    // no hyphen, so this reverses exactly.
    let q = Name::decode(b"\x04a--b\x01x\x07handles\x04link\x00")
        .unwrap()
        .query(&handles_link())
        .unwrap();
    assert_eq!(q.handle(), "a__b");
}

#[test]
fn github_labels_are_the_handle_unchanged() {
    let q = Name::decode(b"\x05alice\x06github\x07handles\x04link\x00")
        .unwrap()
        .query(&handles_link())
        .unwrap();
    assert_eq!(q.handle(), "alice");
}

#[test]
fn gmail_joins_its_labels_and_appends_the_domain_the_platform_implies() {
    let q = Name::decode(b"\x05alice\x01b\x06google\x07handles\x04link\x00")
        .unwrap()
        .query(&handles_link())
        .unwrap();
    assert_eq!(q.handle(), "alice.b@gmail.com");
}

#[test]
fn gmail_refuses_characters_no_account_can_hold() {
    // Our email rules admit `-`, which Gmail does not issue. Refusing is
    // deliberate: a mapping for characters no account holds would be
    // untested code on a payment path.
    assert!(matches!(
        Name::decode(b"\x06al-ice\x06google\x07handles\x04link\x00")
            .unwrap()
            .query(&handles_link()),
        Err(EnsError::UnnormalizedLabel(_))
    ));
}

/// A label holds at most 63 octets, so a name spelling a storage key as
/// 64 hex characters does not decode: names resolve handles, as
/// `resolveHandle` does, never storage keys.
#[test]
fn a_label_past_sixty_three_octets_cannot_be_encoded() {
    let long = "1".repeat(64);
    let mut wire = vec![long.len() as u8];
    wire.extend_from_slice(long.as_bytes());
    wire.extend_from_slice(b"\x07handles\x04link\x00");
    assert_eq!(Name::decode(&wire), Err(EnsError::MalformedName));
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
    let of = |name: &str| named(&name.split('.').collect::<Vec<_>>()).node();
    assert_eq!(named(&[]).node(), B256::ZERO);
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
    let one = named(&["a.b"]).node();
    let two = named(&["a", "b"]).node();
    assert_ne!(one, two);
}

// ─── Coin types ─────────────────────────────────────────────────

#[test]
fn a_chain_gets_the_coin_type_ensip11_gives_it() {
    assert_eq!(
        CoinType::of_chain(1),
        CoinType::from(U256::from(0x8000_0001u64))
    );
    assert_eq!(
        CoinType::of_chain(8453), // Base
        CoinType::from(U256::from(0x8000_2105u64))
    );
}

/// Bare `addr(node)` asks for the chain of the registry it went through.
///
/// MetaMask on Sepolia asks Sepolia's registry with coin type 60 and sends
/// on Sepolia. Read as Ethereum mainnet, that query has no answer in a
/// deployment that indexes Sepolia, and a bound name shows no resolution.
#[test]
fn the_legacy_coin_type_names_the_chain_of_the_registry_asked() {
    const MAINNET: u64 = 1;
    const SEPOLIA: u64 = 11_155_111;
    let legacy = CoinType::from(U256::from(60u64));

    assert!(legacy.names_chain(MAINNET, MAINNET));
    assert!(legacy.names_chain(SEPOLIA, SEPOLIA));
    assert!(!legacy.names_chain(MAINNET, SEPOLIA));
    assert!(!legacy.names_chain(SEPOLIA, MAINNET));

    // An ENSIP-11 coin type names its chain through either registry.
    assert!(CoinType::of_chain(MAINNET).names_chain(MAINNET, SEPOLIA));
    assert!(CoinType::of_chain(SEPOLIA).names_chain(SEPOLIA, MAINNET));
    assert!(!CoinType::of_chain(8453).names_chain(MAINNET, MAINNET));
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
    assert_eq!(CoinType::of_chain(EDEN), CoinType::from(U256::from(EDEN)));
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
    let node = Name::decode(ALICE_X).unwrap().node();
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
    let request = resolve_call(ALICE_X, &addr_call(B256::with_last_byte(1), 0x8000_2105));
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

// ─── Chain names ────────────────────────────────────────────────

/// A chain name is a label the parse can read back, and never a platform
/// key — the parse decides "platform or chain" by the platform keys, so
/// an overlap would make one name mean two things.
#[test]
fn a_chain_name_is_a_label_apart_from_the_platforms() {
    for ok in ["eden", "base", "op-mainnet", "l2"] {
        assert_eq!(
            ok.parse::<ChainName>().map(|n| n.to_string()),
            Ok(ok.to_string())
        );
    }
    for platform in KnownPlatform::ALL {
        let key = platform.key();
        assert_eq!(
            key.parse::<ChainName>(),
            Err(ChainNameError::Platform(key.to_string()))
        );
    }
    for bad in ["", "Base", "eth main", "eth_main", "bäse", "x.y"] {
        assert!(
            matches!(bad.parse::<ChainName>(), Err(ChainNameError::Malformed(_))),
            "{bad:?}"
        );
    }
}
