//! Referral codes: the base58 of a node's leading bytes, and what they name.

use alloy::primitives::B256;

use std::time::{
    Duration,
    Instant,
};

use usernames_core::referral::*;

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
    let longest = Code::from(vec![0xff; 32]).to_string();
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
    let code = |bytes: &[u8]| Code::from(bytes.to_vec());
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
    let lengths: Vec<usize> = Code::prefixes(node).map(|c| c.prefix().len()).collect();
    assert_eq!(lengths, (8..=32).collect::<Vec<_>>());
}
