//! A chain's configuration: what it names and how it is read.

use alloy::{
    primitives::{
        Address,
        B256,
    },
    rpc::types::Log,
};
use libid_contracts::bindings::referral::ReferralSplitter;
use usernames_core::events::NamesEvent;

use alloy::{
    primitives::{
        self,
        U256,
    },
    sol_types::SolEvent,
};

use usernames_core::chain::*;

const SPLITTER: Address = Address::repeat_byte(0x5B);

/// A mined `Referred` log, as `emitter` would emit it.
fn referred_by(emitter: Address) -> Log {
    Log {
        inner: primitives::Log {
            address: emitter,
            data: ReferralSplitter::Referred {
                referrerNode: B256::repeat_byte(0x0A),
                binder: Address::repeat_byte(0xB1),
                sessionId: B256::ZERO,
                referrerPlatformId: B256::ZERO,
                platformId: B256::ZERO,
                handleNode: B256::repeat_byte(0x0B),
                fee: U256::from(10u64),
                share: U256::from(3u64),
                outcome: 0,
            }
            .encode_log_data(),
        },
        block_number: Some(7),
        log_index: Some(3),
        transaction_hash: Some(B256::repeat_byte(0x22)),
        block_timestamp: Some(1_700_000_000),
        ..Log::default()
    }
}

/// A `Referred` counts only from the configured splitter: the same log
/// from the registry, the escrow or any other address is skipped.
#[test]
fn a_referral_is_read_only_from_the_configured_splitter() {
    let contracts = Contracts {
        registry: Address::repeat_byte(0x11),
        escrow: Some(Address::repeat_byte(0xE5)),
        referral_splitter: Some(SPLITTER),
    };
    let decoded = contracts.decode(&referred_by(SPLITTER)).unwrap();
    assert!(matches!(decoded, Some((NamesEvent::Referred { .. }, _))));
    for other in [
        contracts.registry,
        Address::repeat_byte(0xE5),
        Address::repeat_byte(0x5C),
    ] {
        assert!(contracts.decode(&referred_by(other)).unwrap().is_none());
    }
    let unwatched = Contracts {
        referral_splitter: None,
        ..contracts
    };
    assert!(unwatched.decode(&referred_by(SPLITTER)).unwrap().is_none());
}
