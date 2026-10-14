//! The platform registry and node derivation.
//!
//! A platform id is keccak256 of its key. Handle and id nodes are
//! libid-identity's `SHA256(tag || value)`, the nodes the circuits output and
//! the contracts key by; this module only names the platform for it.

use std::{
    collections::HashMap,
    fmt,
    str::FromStr,
    sync::LazyLock,
};

use alloy::primitives::{
    keccak256,
    B256,
};
use libid_identity::{
    handle_vectors::{
        self,
        PLATFORM_GITHUB,
        PLATFORM_GOOGLE,
        PLATFORM_X,
    },
    HandleError,
    IdError,
};
use serde_with::{
    DeserializeFromStr,
    SerializeDisplay,
};

/// keccak256 of a platform key is its id. [`Platform`] is the one entry
/// point for naming platforms, so this stays private: a caller cannot
/// conjure an id from a key the registry does not know.
fn platform_id(key: &str) -> B256 {
    keccak256(key.as_bytes())
}

/// Every platform this build knows.
///
/// A closed set. The id derivation, the normalization rules and the
/// ENS label transform each have to be written once per platform, and an
/// exhaustive `match` on this type is what makes the compiler say so instead of
/// a test noticing later. Adding a platform is a variant here plus its rules in
/// `libid-identity`; everything that must follow stops compiling until it does.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr,
)]
pub enum KnownPlatform {
    /// X, formerly Twitter.
    X,
    /// GitHub.
    GitHub,
    /// Google, whose handles are addresses rather than bare names.
    Google,
}

impl KnownPlatform {
    /// Every variant, in the order a listing shows them. A slice rather than
    /// an array so adding one is a single line and not also an arity.
    pub const ALL: &'static [Self] = &[Self::X, Self::GitHub, Self::Google];

    /// The platform as libid-identity's table states it: key, tags, rules.
    const fn spec(self) -> &'static handle_vectors::Platform {
        match self {
            Self::X => &PLATFORM_X,
            Self::GitHub => &PLATFORM_GITHUB,
            Self::Google => &PLATFORM_GOOGLE,
        }
    }

    /// The short key: the platform key, whose keccak is the platform id, and
    /// what a name, a URL path and the API's JSON all call the platform.
    pub const fn key(self) -> &'static str {
        self.spec().key
    }

    /// The 32-byte id the chain keys this platform by.
    pub fn id(self) -> B256 {
        platform_id(self.key())
    }

    /// The node a typed handle is bound under, or why no handle can be.
    pub fn handle_node(self, raw: &str) -> Result<B256, HandleError> {
        libid_identity::handle_node(self.key(), raw)
            .unwrap_or_else(|| panic!("libid-identity has no node tags for {self}"))
            .map(B256::from)
    }

    /// `text` with A-Z folded to a-z, and its node: the chain's
    /// `handleNodeOf`, which applies no other rule.
    pub fn folded_handle_node(self, text: &str) -> (String, B256) {
        let (folded, node) = libid_identity::folded_handle_node(self.key(), text)
            .unwrap_or_else(|| panic!("libid-identity has no node tags for {self}"));
        (folded, B256::from(node))
    }

    /// The node a user id is bound under, or why no id can be.
    pub fn id_node(self, id: &str) -> Result<B256, IdError> {
        libid_identity::id_node(self.key(), id)
            .unwrap_or_else(|| panic!("libid-identity has no node tags for {self}"))
            .map(B256::from)
    }
}

/// The platform a short key names.
impl FromStr for KnownPlatform {
    type Err = UnknownPlatform;

    fn from_str(key: &str) -> Result<Self, UnknownPlatform> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.key() == key)
            .ok_or_else(|| UnknownPlatform {
                raw: key.to_string(),
            })
    }
}

/// The short key, which the API's JSON and the parse spell the same way.
impl fmt::Display for KnownPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

static KNOWN_BY_ID: LazyLock<HashMap<B256, KnownPlatform>> =
    LazyLock::new(|| KnownPlatform::ALL.iter().map(|p| (p.id(), *p)).collect());

/// A platform reference that is neither a known key nor a 0x-hex 32-byte id.
/// The message derives the key list from the registry, so it cannot rot as
/// platforms are added.
#[derive(Debug, thiserror::Error)]
#[error(
    "unknown platform {raw:?}: use {keys}or a 0x-hex platform id",
    keys = KnownPlatform::ALL.iter().map(|p| format!("{}, ", p.key())).collect::<String>()
)]
pub struct UnknownPlatform {
    raw: String,
}

/// A platform named by its short key ('x') or its 0x-hex 32-byte id. Only a
/// platform this build knows hashes text into nodes.
#[derive(Debug, Clone, Copy)]
pub struct Platform {
    id: B256,
    known: Option<KnownPlatform>,
}

impl From<KnownPlatform> for Platform {
    fn from(known: KnownPlatform) -> Self {
        Self {
            id: known.id(),
            known: Some(known),
        }
    }
}

/// A caller-supplied reference: a known key, or a 0x-hex id — which picks its
/// key back up when this build knows the platform.
impl FromStr for Platform {
    type Err = UnknownPlatform;

    fn from_str(raw: &str) -> Result<Self, UnknownPlatform> {
        if let Ok(known) = raw.parse::<KnownPlatform>() {
            return Ok(Self::from(known));
        }
        if let Ok(id) = raw.parse::<B256>() {
            return Ok(Self {
                id,
                known: Self::known_of(id),
            });
        }
        Err(UnknownPlatform {
            raw: raw.to_string(),
        })
    }
}

/// The short key when this build knows the platform, its 0x-hex id when
/// not: the forms the parse reads back.
impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.known {
            Some(known) => known.fmt(f),
            None => self.id.fmt(f),
        }
    }
}

impl Platform {
    /// The 32-byte id the chain keys this platform by.
    pub fn id(&self) -> B256 {
        self.id
    }

    /// The platform itself, when this build knows the id.
    pub fn known(&self) -> Option<KnownPlatform> {
        self.known
    }

    /// The platform for a bare id — for rows read back from the database,
    /// where only the id survives. An id this build does not know still
    /// indexes; it just has no rules or name on this side.
    pub fn known_of(id: B256) -> Option<KnownPlatform> {
        KNOWN_BY_ID.get(&id).copied()
    }

    /// The short key for a bare id, when this build knows it.
    pub fn key_of(id: B256) -> Option<&'static str> {
        Self::known_of(id).map(KnownPlatform::key)
    }
}

/// A search query as search matches it: spaces trimmed, one leading `@`
/// stripped, A-Z folded to a-z. Nothing is refused, so a partial or typed
/// handle such as ` @Ali` finds `alice`.
pub fn fold_search_query(raw: &str) -> String {
    let trimmed = raw.trim_matches(' ');
    let stripped = trimmed.strip_prefix('@').unwrap_or(trimmed);
    stripped.to_ascii_lowercase()
}

/// Handle text an event disclosed: a token a circuit hashes, folded, and
/// checked to hash to the node the event names. [`DisclosedHandle::verify`] is the only way to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisclosedHandle(String);

impl DisclosedHandle {
    /// `text` folded, when a circuit would hash it and it hashes to
    /// `handle_node`. Text no circuit hashes no account can bind.
    pub fn verify(
        platform: KnownPlatform,
        handle_node: B256,
        text: &str,
    ) -> Option<Self> {
        let normalized = libid_identity::normalize(text, platform.spec().rules).ok()?;
        let (normalized, node) = platform.folded_handle_node(&normalized);
        (node == handle_node).then_some(Self(normalized))
    }

    /// The normalized text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::hex;
    use std::collections::BTreeSet;

    use super::*;

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
        let id = KnownPlatform::GitHub.id_node("583231").unwrap();
        assert_eq!(id[..4], [0x47, 0x59, 0x02, 0xb2]);
        assert_eq!(
            KnownPlatform::X.handle_node("ali\"ce"),
            Err(HandleError::BadCharacter)
        );
        assert_eq!(KnownPlatform::X.id_node(" 123"), Err(IdError::BadCharacter));
    }

    /// Every known platform has node tags, so the node lookups never panic.
    #[test]
    fn every_known_platform_is_tagged() {
        for platform in KnownPlatform::ALL {
            assert!(
                libid_identity::tags_for(platform.key()).is_some(),
                "{platform}"
            );
        }
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
}
