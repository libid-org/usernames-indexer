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
    platforms::{
        self,
        PLATFORM_GITHUB,
        PLATFORM_GOOGLE,
        PLATFORM_TELEGRAM,
        PLATFORM_X,
    },
    HandleError,
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
    /// Telegram.
    Telegram,
}

impl KnownPlatform {
    /// Every variant, in the order a listing shows them. A slice rather than
    /// an array so adding one is a single line and not also an arity.
    pub const ALL: &'static [Self] =
        &[Self::X, Self::GitHub, Self::Google, Self::Telegram];

    /// The platform as libid-identity's table states it: key, tags, rules.
    const fn spec(self) -> &'static platforms::Platform {
        match self {
            Self::X => &PLATFORM_X,
            Self::GitHub => &PLATFORM_GITHUB,
            Self::Google => &PLATFORM_GOOGLE,
            Self::Telegram => &PLATFORM_TELEGRAM,
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
        self.spec().handle_node(raw).map(B256::from)
    }

    /// `text` with A-Z folded to a-z, and its node: the chain's
    /// `handleNodeOf`, which applies no other rule.
    pub fn folded_handle_node(self, text: &str) -> (String, B256) {
        let (folded, node) = self.spec().folded_handle_node(text);
        (folded, B256::from(node))
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
