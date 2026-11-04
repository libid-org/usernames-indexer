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
/// store's to say: of the bound nodes those bytes start, the one bound
/// first.
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

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(&self.0).into_string())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{
        Duration,
        Instant,
    };

    use super::*;

    #[test]
    fn a_code_reads_back_into_its_bytes_leading_zeros_included() {
        let node = B256::left_padding_from(&[0xab; 29]);
        for code in Code::prefixes(node) {
            let text = code.to_string();
            assert_eq!(text.parse::<Code>().unwrap(), code, "{text}");
        }
        let shortest = Code::prefixes(node).next().unwrap();
        assert_eq!(shortest.prefix(), &[0, 0, 0, 0xab, 0xab, 0xab, 0xab, 0xab]);
        assert!(shortest.to_string().starts_with("111"), "{shortest}");
    }

    #[test]
    fn a_code_of_fewer_than_8_or_more_than_32_bytes_is_refused() {
        for len in [0, 1, 7, 33, 64] {
            let text = bs58::encode(vec![0x42; len]).into_string();
            assert!(text.parse::<Code>().is_err(), "{len} bytes: {text:?}");
        }
        for text in ["0OIl0OIl0OIl", "not base58!", ""] {
            assert!(text.parse::<Code>().is_err(), "{text:?}");
        }
    }

    #[test]
    fn the_longest_code_reads_and_longer_text_is_refused_before_decoding() {
        let longest = Code(vec![0xff; 32]).to_string();
        assert_eq!(longest.len(), Code::MAX_CHARS);
        assert!(longest.parse::<Code>().is_ok());

        for len in [Code::MAX_CHARS + 1, 16_000, 64 * 1024] {
            let long = "z".repeat(len);
            let started = Instant::now();
            assert!(long.parse::<Code>().is_err(), "{len} characters");
            let took = started.elapsed();
            assert!(
                took < Duration::from_millis(50),
                "{len} characters: {took:?}"
            );
        }
    }

    #[test]
    fn past_a_prefix_is_its_last_byte_below_0xff_raised() {
        let code = |bytes: &[u8]| Code(bytes.to_vec());
        assert_eq!(
            code(&[1, 2, 3, 4, 5, 6, 7, 8]).past(),
            Some(vec![1, 2, 3, 4, 5, 6, 7, 9])
        );
        assert_eq!(
            code(&[1, 2, 3, 4, 5, 6, 0xff, 0xff]).past(),
            Some(vec![1, 2, 3, 4, 5, 7])
        );
        assert_eq!(code(&[0xff; 8]).past(), None);
    }

    #[test]
    fn a_node_goes_by_25_codes_from_8_bytes_to_all_32() {
        let node = B256::repeat_byte(0x5a);
        let lengths: Vec<usize> =
            Code::prefixes(node).map(|c| c.prefix().len()).collect();
        assert_eq!(lengths, (8..=32).collect::<Vec<_>>());
    }
}
