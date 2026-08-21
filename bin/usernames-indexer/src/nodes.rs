//! Node derivation, mirroring `IdentityNodes.sol`.
//!
//! Every key in the contract's storage is a keccak of three 32-byte words:
//! a version tag, the platform id, and the keccak of the string. `abi.encode`
//! of three static `bytes32` values is their plain concatenation, so no ABI
//! machinery is needed here.
//!
//! The indexer recomputes both nodes for every `IdentityBound` it applies and
//! compares them against the emitted topics. A mismatch means this build's
//! constants or normalizer drifted from the chain — it is reported loudly and
//! the emitted topics win, because the chain already keyed its storage.

use std::{
    collections::HashMap,
    sync::LazyLock,
};

use alloy::primitives::{
    keccak256,
    B256,
};
use libid_identity::handle_vectors::{
    PLATFORM_GITHUB_DOMAIN,
    PLATFORM_GOOGLE_DOMAIN,
    PLATFORM_X_DOMAIN,
};

/// keccak256 of a platform's domain string is its id.
pub fn platform_id(domain: &str) -> B256 {
    keccak256(domain.as_bytes())
}

static ID_NODE_V1: LazyLock<B256> =
    LazyLock::new(|| keccak256(b"dyaka.identity.id-node.v1"));
static HANDLE_NODE_V1: LazyLock<B256> =
    LazyLock::new(|| keccak256(b"dyaka.identity.handle-node.v1"));

fn node(tag: B256, platform: B256, inner: B256) -> B256 {
    let mut buf = [0u8; 96];
    buf[..32].copy_from_slice(tag.as_slice());
    buf[32..64].copy_from_slice(platform.as_slice());
    buf[64..].copy_from_slice(inner.as_slice());
    keccak256(buf)
}

/// The storage key for an account id. The id is hashed byte-verbatim: the
/// contract never normalizes it, so neither does this.
pub fn id_node(platform: B256, user_id: &str) -> B256 {
    node(*ID_NODE_V1, platform, keccak256(user_id.as_bytes()))
}

/// The storage key for a handle. The handle must already be normalized —
/// hashing a raw handle writes a key no reader will find.
pub fn handle_node(platform: B256, normalized_handle: &str) -> B256 {
    node(
        *HANDLE_NODE_V1,
        platform,
        keccak256(normalized_handle.as_bytes()),
    )
}

/// platform id -> the short key handles.json names it by ('x', 'github',
/// 'google'). An id outside this table still indexes; it just has no
/// normalization rules on this side.
pub static KNOWN_PLATFORMS: LazyLock<HashMap<B256, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        (platform_id(PLATFORM_X_DOMAIN), "x"),
        (platform_id(PLATFORM_GITHUB_DOMAIN), "github"),
        (platform_id(PLATFORM_GOOGLE_DOMAIN), "google"),
    ])
});

/// The platform id for a short key, the inverse of [`KNOWN_PLATFORMS`].
pub fn platform_id_for_key(key: &str) -> Option<B256> {
    match key {
        "x" => Some(platform_id(PLATFORM_X_DOMAIN)),
        "github" => Some(platform_id(PLATFORM_GITHUB_DOMAIN)),
        "google" => Some(platform_id(PLATFORM_GOOGLE_DOMAIN)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::hex;

    use super::*;

    // Fixed points computed independently with `cast keccak`, so a drifted
    // constant or a broken concatenation fails against numbers this code
    // never produced.
    #[test]
    fn platform_ids_match_the_chain() {
        let cases = [
            (
                "x",
                "c35cd221f653c2881299fd15eafb60135268d945ad58b7bdbceefea1e2274375",
            ),
            (
                "github",
                "6e6b76ab962fcaebc5bd2f6ba8868866bc6159e1d6481c3376a7383217a84337",
            ),
            (
                "google",
                "c96cebdfd032a45e92ae16d7e629a9b0e5781858be9e7a108f37acdcc2347b08",
            ),
        ];
        for (key, expected) in cases {
            let got = platform_id_for_key(key).unwrap();
            assert_eq!(hex::encode(got), expected, "platform {key}");
        }
    }

    #[test]
    fn nodes_match_the_chain() {
        let x = platform_id_for_key("x").unwrap();
        let google = platform_id_for_key("google").unwrap();
        assert_eq!(
            hex::encode(handle_node(x, "alice_1")),
            "46a9cdc4134b422003a300cf0b1b318812b07e1f11cec6f5240a3dcf63fa745f"
        );
        assert_eq!(
            hex::encode(id_node(x, "12345")),
            "6d6f828075247b9577269f5991a029e7cf90da5c388e62a4f1b4359da480a5a5"
        );
        assert_eq!(
            hex::encode(handle_node(google, "a.b+tag@example.com")),
            "510cacc46b1c93510291235a2326a96fbb5fa3ff9ba71e1fcc1fba3e27d2d45c"
        );
    }
}
