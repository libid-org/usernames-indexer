//! Platforms and handle nodes: the ids and hashes the chain and the
//! circuits compute, and the text a node discloses.

use std::collections::BTreeSet;

use alloy::primitives::{
    hex,
    B256,
};
use libid_identity::HandleError;
use usernames_core::nodes::*;

// Fixed points computed independently with `cast keccak`, so a drifted
// constant or a broken concatenation fails against numbers this code
// never produced.
#[test]
fn platform_ids_match_the_chain() {
    let cases = [
        (
            "x",
            "7521d1cadbcfa91eec65aa16715b94ffc1c9654ba57ea2ef1a2127bca1127a83",
        ),
        (
            "github",
            "07a17bd3c7c8d7b88e93a4d9007e3bc230b0a586a434de0bed6500e9f343deb7",
        ),
        (
            "google",
            "8f2f90d8304f6eb382d037c47a041d8c8b4d18bdd8b082fa32828e016a584ca7",
        ),
    ];
    for (key, expected) in cases {
        let got = key.parse::<Platform>().unwrap().id();
        assert_eq!(hex::encode(got), expected, "platform {key}");
    }
}

/// libid-identity's README vectors, from `hashlib.sha256` over tag and
/// text: numbers this crate never produced.
#[test]
fn nodes_match_the_circuits() {
    let node = KnownPlatform::GitHub.handle_node("OctoCat").unwrap();
    assert_eq!(node[..4], [0x38, 0x1f, 0xb9, 0xd7]);
    assert_eq!(
        KnownPlatform::X.handle_node("ali\"ce"),
        Err(HandleError::BadCharacter)
    );
}

#[test]
fn disclosed_text_folds_and_must_hash_to_its_node() {
    let x = KnownPlatform::X;
    let node = x.handle_node("alice_1").unwrap();
    let disclosed = DisclosedHandle::verify(x, node, "Alice_1").unwrap();
    assert_eq!(disclosed.as_str(), "alice_1");
    assert!(DisclosedHandle::verify(x, node, "alice_2").is_none());
    assert!(DisclosedHandle::verify(x, node, "alice_1!").is_none());
    let github = KnownPlatform::GitHub;
    assert!(DisclosedHandle::verify(github, node, "alice_1").is_none());
}

/// Text no circuit hashes is not a handle, even when its fold-only node,
/// as `depositToHandle` keys it, matches.
#[test]
fn text_no_circuit_hashes_is_not_disclosed() {
    let x = KnownPlatform::X;
    for text in ["Bo\"b", "Bo\0b"] {
        let (_, node) = x.folded_handle_node(text);
        assert!(DisclosedHandle::verify(x, node, text).is_none(), "{text:?}");
    }
}

/// The registry is the launch profile set, no more and no less: every
/// platform a ceremony can prove is one this side can name, and nothing
/// is named here that no ceremony proves.
#[test]
fn the_registry_is_the_launch_profile_set() {
    let launch: BTreeSet<&str> =
        libid_profiles::LAUNCH.iter().map(|p| p.platform).collect();
    let known: BTreeSet<&str> = KnownPlatform::ALL.iter().map(|p| p.key()).collect();
    assert_eq!(known, launch);
}

#[test]
fn parse_accepts_keys_and_hex_and_names_the_keys_it_knows() {
    let by_key = "x".parse::<Platform>().unwrap();
    assert_eq!(by_key.known(), Some(KnownPlatform::X));

    // The hex form of a registered platform picks its key back up.
    let by_id = by_key.id().to_string().parse::<Platform>().unwrap();
    assert_eq!(by_id.id(), by_key.id());
    assert_eq!(by_id.known(), Some(KnownPlatform::X));

    // An unregistered id still parses — events are self-describing —
    // it just has no key, and so no rules, on this side.
    let foreign = B256::repeat_byte(7)
        .to_string()
        .parse::<Platform>()
        .unwrap();
    assert_eq!(foreign.known(), None);

    // The refusal names every registered key, derived from the registry.
    let err = "myspace".parse::<Platform>().unwrap_err().to_string();
    for platform in KnownPlatform::ALL {
        assert!(err.contains(platform.key()), "{err}");
    }
}
#[test]
fn a_platform_prints_what_its_parse_reads_back() {
    let foreign = B256::repeat_byte(7).to_string();
    for raw in ["x", "github", "google", foreign.as_str()] {
        let platform: Platform = raw.parse().unwrap();
        assert_eq!(platform.to_string(), raw);
    }
    for known in KnownPlatform::ALL {
        assert_eq!(
            known.to_string().parse::<KnownPlatform>().ok(),
            Some(*known)
        );
    }
}
