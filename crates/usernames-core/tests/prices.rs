//! Prices: the price source's answers, read into quotes.

use alloy::primitives::{
    Address,
    U512,
};
use serde_json::value::RawValue;
use usernames_core::{
    chain::NATIVE_COIN,
    db::{
        Candidate,
        ListedAddress,
    },
};

use alloy::primitives::U256;

use usernames_core::api::prices::*;

fn price(text: &str) -> U512 {
    text.parse::<Price>().expect("a price").0
}

#[test]
fn a_price_parses_exactly_in_either_notation() {
    let unit = U512::from(10u8).pow(U512::from(18u8));
    assert_eq!(price("1"), unit);
    assert_eq!(
        price("3412.53"),
        U512::from(341_253u64) * unit / U512::from(100u8)
    );
    assert_eq!(price("1.2e-5"), U512::from(12_000_000_000_000u64));
    assert_eq!(price("4.732E-06"), U512::from(4_732_000_000_000u64));
    for refused in ["", "-1", "1.2.3", "abc", "1e", "1e-99999999999"] {
        assert!(refused.parse::<Price>().is_err(), "{refused}");
    }
}

#[test]
fn a_price_source_parses_back_from_its_display() {
    let source: PriceSource = "defillama".parse().expect("a source");
    assert_eq!(source, PriceSource::default());
    assert_eq!(source.to_string(), "defillama");
    for refused in ["", "DefiLlama", "https://coins.llama.fi"] {
        assert!(refused.parse::<PriceSource>().is_err(), "{refused}");
    }
}

/// Entries of DefiLlama's `/v2/chains` as served on 2026-11-15.
fn listed_chains() -> ChainNames {
    let listed: Vec<ListedChain> =
        serde_json::from_str(include_str!("fixtures/llama_chains.json"))
            .expect("the list");
    ListedChain::names(listed)
}

#[test]
fn listed_chains_are_named_lowercased_by_chain_id() {
    let chains = listed_chains();
    assert_eq!(chains.name(1), Some("ethereum"));
    assert_eq!(chains.name(8453), Some("base"));
    assert_eq!(
        chains.name(56),
        Some("binance"),
        "the least of BSC and Binance"
    );
    assert_eq!(
        chains.name(10),
        Some("op mainnet"),
        "the least of OP Mainnet and Optimism"
    );
    assert_eq!(chains.name(11235), Some("haqq"), "an id listed as a string");
    assert_eq!(chains.name(84532), None, "a testnet");
    assert_eq!(chains.0.len(), 5, "entries without an id name nothing");
}

#[test]
fn a_coin_id_names_the_chain_and_the_source_coin() {
    let chains = listed_chains();
    let usdc = ListedAddress {
        chain_id: 8453,
        address: Address::repeat_byte(0xAB),
    };
    assert_eq!(
        PriceSource::DefiLlama.coin_id(&chains, &usdc).as_deref(),
        Some("base:0xabababababababababababababababababababab")
    );
    let coin = ListedAddress {
        chain_id: 1,
        address: NATIVE_COIN,
    };
    assert_eq!(
        PriceSource::DefiLlama.coin_id(&chains, &coin).as_deref(),
        Some("ethereum:0x0000000000000000000000000000000000000000")
    );
    let testnet = ListedAddress {
        chain_id: 84532,
        address: NATIVE_COIN,
    };
    assert_eq!(PriceSource::DefiLlama.coin_id(&chains, &testnet), None);
}

#[test]
fn a_symbol_is_served_only_short_and_without_control_characters() {
    let symbol = |symbol: &str| {
        Priced {
            price: RawValue::from_string("1".to_string()).expect("a number"),
            decimals: Some(6),
            symbol: Some(symbol.to_string()),
            confidence: Some(0.99),
            timestamp: 0,
        }
        .symbol()
    };
    assert_eq!(symbol("USDC").as_deref(), Some("USDC"));
    assert_eq!(symbol(&"A".repeat(33)), None);
    assert_eq!(symbol("US\u{7}DC"), None);
    assert_eq!(symbol("US\nDC"), None);
    assert_eq!(symbol("US\u{202E}DC"), None);
    assert_eq!(symbol("US\u{200B}DC"), None);
    assert_eq!(symbol("US\u{2066}DC"), None);
}

/// A known coin of chain 1 with 18 decimals, priced at `dollars` at
/// unix second `at`, `current` or not.
fn known(token: Address, dollars: u64, at: i64, current: bool) -> (ListedAddress, Known) {
    let coin = ListedAddress {
        chain_id: 1,
        address: token,
    };
    let quote = Quote {
        symbol: None,
        decimals: 18,
        price: Price(U512::from(dollars) * U512::from(10u8).pow(U512::from(18u8))),
        at,
    };
    (coin, Known { quote, current })
}

#[test]
fn coins_without_a_price_cannot_push_out_those_with_one() {
    let now = unix_now();
    let real = Address::repeat_byte(0x05);
    let mut book = Book::default();
    let mut candidates = Vec::new();
    let mut add = |(coin, known): (ListedAddress, Known), moved: &str| {
        book.0.insert(coin, known);
        candidates.push(Candidate {
            coin,
            moved: moved.to_string(),
        });
    };
    add(known(NATIVE_COIN, 3_000, now, true), "10");
    add(known(real, 1, now, true), "10");
    for i in 0..KEPT_PER_CHAIN as u64 {
        let lapsed = Address::from_word(U256::from(1_000 + i).into());
        add(
            known(lapsed, 1, now - 2 * 24 * 60 * 60, false),
            "1000000000000000000000000",
        );
    }
    book.keep(&candidates);
    let quotes = book.quotes();
    assert!(
        quotes.0.contains_key(&candidates[0].coin),
        "the chain's coin"
    );
    assert!(
        quotes.0.contains_key(&candidates[1].coin),
        "the valued token"
    );
    assert_eq!(book.0.len(), 1 + KEPT_PER_CHAIN);
}
