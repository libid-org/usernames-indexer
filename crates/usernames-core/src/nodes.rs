//! The platform registry and node derivation, mirroring `IdentityNodes.sol`.
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

/// keccak256 of a platform's domain string is its id. Private on purpose:
/// [`Platform`] is the one entry point for naming platforms, so a caller
/// cannot conjure an id from a domain the registry does not know.
fn platform_id(domain: &str) -> B256 {
    keccak256(domain.as_bytes())
}

/// Every platform this build knows, as (short key, domain). The single
/// listing both lookup directions, the rules lookup and the parse error
/// derive from — so adding a platform is one line here (plus its rules in
/// handles.json via `libid-identity`), not three places that must agree.
const REGISTRY: &[(&str, &str)] = &[
    ("x", PLATFORM_X_DOMAIN),
    ("github", PLATFORM_GITHUB_DOMAIN),
    ("google", PLATFORM_GOOGLE_DOMAIN),
];

static KEY_BY_ID: LazyLock<HashMap<B256, &'static str>> = LazyLock::new(|| {
    REGISTRY
        .iter()
        .map(|(key, domain)| (platform_id(domain), *key))
        .collect()
});

/// A platform reference that is neither a known key nor a 0x-hex 32-byte id.
/// The message derives the key list from the registry, so it cannot rot as
/// platforms are added.
#[derive(Debug)]
pub struct UnknownPlatform {
    raw: String,
}

impl std::fmt::Display for UnknownPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown platform {:?}: use ", self.raw)?;
        for (key, _) in REGISTRY {
            write!(f, "{key}, ")?;
        }
        write!(f, "or a 0x-hex platform id")
    }
}

impl std::error::Error for UnknownPlatform {}

/// A platform named by its short key ('x') or its 0x-hex 32-byte id. The key
/// form also carries this build's normalization rules; the hex form indexes
/// fine but string queries run un-normalized.
#[derive(Debug, Clone, Copy)]
pub struct Platform {
    id: B256,
    key: Option<&'static str>,
}

impl Platform {
    /// The platform for a short key from the registry.
    pub fn from_key(key: &str) -> Option<Self> {
        REGISTRY
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(key, domain)| Self {
                id: platform_id(domain),
                key: Some(key),
            })
    }

    /// A caller-supplied reference: a known key, or a 0x-hex id — which
    /// picks its key back up when this build knows the platform.
    pub fn parse(raw: &str) -> Result<Self, UnknownPlatform> {
        if let Some(platform) = Self::from_key(raw) {
            return Ok(platform);
        }
        if let Ok(id) = raw.parse::<B256>() {
            return Ok(Self {
                id,
                key: Self::key_of(id),
            });
        }
        Err(UnknownPlatform {
            raw: raw.to_string(),
        })
    }

    /// The 32-byte id the chain keys this platform by.
    pub fn id(&self) -> B256 {
        self.id
    }

    /// The short key, when this build knows the id.
    pub fn key(&self) -> Option<&'static str> {
        self.key
    }

    /// The short key for a bare id — for rows read back from the database,
    /// where only the id survives. An id outside the registry still indexes;
    /// it just has no normalization rules or name on this side.
    pub fn key_of(id: B256) -> Option<&'static str> {
        KEY_BY_ID.get(&id).copied()
    }

    /// Normalize a full handle exactly the way the chain did before it keyed
    /// the handle. A platform this build has no rules for passes the text
    /// through verbatim — its handles were still stored normalized, the
    /// caller just has to supply that form. An error means the platform could
    /// never hold this text at all.
    pub fn normalize_query(
        &self,
        raw: &str,
    ) -> Result<NormalizedHandle, libid_identity::HandleError> {
        match self.key.and_then(libid_identity::Rules::for_platform) {
            Some(rules) => libid_identity::normalize(raw, rules).map(NormalizedHandle),
            None => Ok(NormalizedHandle(raw.to_string())),
        }
    }
}

/// A handle in the exact byte form the chain keyed. The only ways to get one
/// are [`Platform::normalize_query`] — this side running the chain's rules —
/// and [`NormalizedHandle::from_chain`] for text the chain itself emitted,
/// which is normalized by definition. What cannot happen is hashing raw user
/// input by accident: [`handle_node`] refuses plain strings, because a raw
/// handle writes a key no reader will find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedHandle(String);

impl NormalizedHandle {
    /// Text the chain already keyed: an event field, or a column derived
    /// from one. The chain is the authority on normalization; this side does
    /// not second-guess what it emitted.
    pub fn from_chain(handle: &str) -> Self {
        Self(handle.to_string())
    }

    /// The normalized text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fold a search query the way normalization would, without refusing partial
/// input: trim spaces, strip one leading `@`, lowercase A-Z. A partial handle
/// cannot pass shape checks, so full normalization is deliberately not run —
/// which is also why this returns a plain `String` and not a
/// [`NormalizedHandle`]: folded text is not chain-keyed text.
pub fn fold_search_query(raw: &str) -> String {
    let trimmed = raw.trim_matches(' ');
    let stripped = trimmed.strip_prefix('@').unwrap_or(trimmed);
    stripped.to_ascii_lowercase()
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

/// The storage key for a handle. Demanding [`NormalizedHandle`] instead of a
/// plain string is what keeps "hashed a raw handle" a compile error rather
/// than a key no reader will ever find.
pub fn handle_node(platform: B256, handle: &NormalizedHandle) -> B256 {
    node(
        *HANDLE_NODE_V1,
        platform,
        keccak256(handle.as_str().as_bytes()),
    )
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
            let got = Platform::from_key(key).unwrap().id();
            assert_eq!(hex::encode(got), expected, "platform {key}");
        }
    }

    #[test]
    fn nodes_match_the_chain() {
        let x = Platform::from_key("x").unwrap().id();
        let google = Platform::from_key("google").unwrap().id();
        assert_eq!(
            hex::encode(handle_node(x, &NormalizedHandle::from_chain("alice_1"))),
            "46a9cdc4134b422003a300cf0b1b318812b07e1f11cec6f5240a3dcf63fa745f"
        );
        assert_eq!(
            hex::encode(id_node(x, "12345")),
            "6d6f828075247b9577269f5991a029e7cf90da5c388e62a4f1b4359da480a5a5"
        );
        assert_eq!(
            hex::encode(handle_node(
                google,
                &NormalizedHandle::from_chain("a.b+tag@example.com")
            )),
            "510cacc46b1c93510291235a2326a96fbb5fa3ff9ba71e1fcc1fba3e27d2d45c"
        );
    }

    #[test]
    fn parse_accepts_keys_and_hex_and_names_the_keys_it_knows() {
        let by_key = Platform::parse("x").unwrap();
        assert_eq!(by_key.key(), Some("x"));

        // The hex form of a registered platform picks its key back up.
        let by_id = Platform::parse(&by_key.id().to_string()).unwrap();
        assert_eq!(by_id.id(), by_key.id());
        assert_eq!(by_id.key(), Some("x"));

        // An unregistered id still parses — events are self-describing —
        // it just has no key, and so no rules, on this side.
        let foreign = Platform::parse(&B256::repeat_byte(7).to_string()).unwrap();
        assert_eq!(foreign.key(), None);

        // The refusal names every registered key, derived from the registry.
        let err = Platform::parse("myspace").unwrap_err().to_string();
        for (key, _) in REGISTRY {
            assert!(err.contains(key), "{err}");
        }
    }
}
