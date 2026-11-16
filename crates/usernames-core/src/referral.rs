//! Referral codes: a handle node's first bytes in base58, short enough for a
//! link, naming the node without its text.

use std::{
    fmt,
    ops::RangeInclusive,
    str::FromStr,
};

use alloy::primitives::B256;
use serde_with::{
    DeserializeFromStr,
    SerializeDisplay,
};

/// A referral code: the base58 of a handle node's first 8 to 32 bytes,
/// leading zero bytes kept as leading `1`s. Which node a code names is the
/// store's to say: the one bound node those bytes start, and none while two
/// do.
#[derive(Clone, Debug, PartialEq, Eq, SerializeDisplay, DeserializeFromStr)]
pub struct Code(Vec<u8>);

/// Text that is not a referral code.
#[derive(Debug, thiserror::Error)]
#[error("not a referral code: the base58 of 8 to 32 bytes")]
pub struct InvalidCode;

impl Code {
    /// The bytes of its node a code carries. At 8 bytes, about 11
    /// characters, two of a million nodes share one with odds of about 3e-8.
    const LENGTHS: RangeInclusive<usize> = 8..=32;
    /// The longest code: 32 bytes of 0xff in base58. Text past it is refused
    /// before decoding, whose cost grows with the square of the length.
    #[cfg_attr(feature = "test-helpers", visibility::make(pub))]
    const MAX_CHARS: usize = 44;

    /// Every code `node` may go by, shortest first, the whole node last.
    pub fn prefixes(node: B256) -> impl Iterator<Item = Self> {
        Self::LENGTHS.map(move |len| Self(node[..len].to_vec()))
    }

    /// The bytes every node this code may name starts with.
    pub fn prefix(&self) -> &[u8] {
        &self.0
    }

    /// The least byte string past every node the prefix starts: the prefix
    /// up to its last byte below 0xff, that byte raised by one. `None` for a
    /// prefix of all 0xff, which no byte string follows.
    pub fn past(&self) -> Option<Vec<u8>> {
        let mut past = self.0.clone();
        while let Some(last) = past.pop() {
            if last < u8::MAX {
                past.push(last + 1);
                return Some(past);
            }
        }
        None
    }
}

impl FromStr for Code {
    type Err = InvalidCode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() > Self::MAX_CHARS {
            return Err(InvalidCode);
        }
        let bytes = bs58::decode(s).into_vec().map_err(|_| InvalidCode)?;
        if !Self::LENGTHS.contains(&bytes.len()) {
            return Err(InvalidCode);
        }
        Ok(Self(bytes))
    }
}

/// The code of `bytes`, whatever their length, as the suites build it.
#[cfg(feature = "test-helpers")]
impl From<Vec<u8>> for Code {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(&self.0).into_string())
    }
}
