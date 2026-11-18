//! A chain's configuration: what it names and how it is read.

use alloy::{
    contract,
    primitives::{
        Address,
        B256,
    },
    rpc::types::Log,
};
use libid_contracts::bindings::{
    escrow::HandleEscrow,
    identity::IdentityRegistry,
    referral::ReferralSplitter,
};
use usernames_core::events::NamesEvent;

use alloy::{
    primitives::{
        self,
        LogData,
        U256,
    },
    sol_types::SolEvent,
};

use usernames_core::chain::*;

const SPLITTER: Address = Address::repeat_byte(0x5B);

const REGISTRY: Address = Address::repeat_byte(0x11);
const ESCROW: Address = Address::repeat_byte(0xE5);

/// The three contracts, all watched.
const CONTRACTS: Contracts = Contracts {
    registry: REGISTRY,
    escrow: Some(ESCROW),
    referral_splitter: Some(SPLITTER),
};

/// A mined log of `data`, as `emitter` would emit it.
fn mined(emitter: Address, data: LogData) -> Log {
    Log {
        inner: primitives::Log {
            address: emitter,
            data,
        },
        block_number: Some(7),
        log_index: Some(3),
        transaction_hash: Some(B256::repeat_byte(0x22)),
        block_timestamp: Some(1_700_000_000),
        ..Log::default()
    }
}

/// A mined `Referred` log, as `emitter` would emit it.
fn referred_by(emitter: Address) -> Log {
    mined(
        emitter,
        ReferralSplitter::Referred {
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
    )
}

/// A `Referred` counts only from the configured splitter: the same log
/// from the registry, the escrow or any other address is skipped.
#[test]
fn a_referral_is_read_only_from_the_configured_splitter() {
    let contracts = CONTRACTS;
    let decoded = contracts.decode(&referred_by(SPLITTER)).unwrap();
    assert!(matches!(decoded, Some((NamesEvent::Referred { .. }, _))));
    for other in [contracts.registry, ESCROW, Address::repeat_byte(0x5C)] {
        assert!(contracts.decode(&referred_by(other)).unwrap().is_none());
    }
    let unwatched = Contracts {
        referral_splitter: None,
        ..contracts
    };
    assert!(unwatched.decode(&referred_by(SPLITTER)).unwrap().is_none());
}

/// An event a watched contract declares and the index does not read is
/// named, from that contract only; any other topic is not.
#[test]
fn an_unread_event_is_named_by_the_contract_that_declares_it() {
    let upgraded = IdentityRegistry::Upgraded {
        implementation: Address::repeat_byte(0x1A),
    }
    .encode_log_data();
    let initialized = HandleEscrow::Initialized { version: 1 }.encode_log_data();
    let transferred = ReferralSplitter::OwnershipTransferred {
        previousOwner: Address::ZERO,
        newOwner: Address::repeat_byte(0x0E),
    }
    .encode_log_data();
    let share_set = ReferralSplitter::ShareSet { shareBps: 3000 }.encode_log_data();

    assert_eq!(
        CONTRACTS.unread(&mined(REGISTRY, upgraded.clone())),
        Some("Upgraded(address)")
    );
    assert_eq!(
        CONTRACTS.unread(&mined(ESCROW, initialized)),
        Some("Initialized(uint64)")
    );
    assert_eq!(
        CONTRACTS.unread(&mined(SPLITTER, transferred)),
        Some("OwnershipTransferred(address,address)")
    );
    assert_eq!(
        CONTRACTS.unread(&mined(SPLITTER, share_set.clone())),
        Some("ShareSet(uint16)")
    );

    assert_eq!(CONTRACTS.unread(&mined(REGISTRY, share_set)), None);
    assert_eq!(CONTRACTS.unread(&mined(SPLITTER, upgraded)), None);
    let undeclared =
        LogData::new_unchecked(vec![B256::repeat_byte(0x7E)], Default::default());
    assert_eq!(CONTRACTS.unread(&mined(REGISTRY, undeclared)), None);
}

/// Every event a watched contract's bindings declare is either read by
/// `decode` or named as unread, never both: a bindings bump that adds an
/// event fails here until the index reads it or lists it. A read event's
/// empty payload fails to decode, so `Ok(None)` marks exactly the skipped
/// topics.
#[test]
fn every_declared_event_is_read_or_named_unread() {
    let watched = [
        (
            REGISTRY,
            IdentityRegistry::IdentityRegistryEvents::SELECTORS,
        ),
        (ESCROW, HandleEscrow::HandleEscrowEvents::SELECTORS),
        (
            SPLITTER,
            ReferralSplitter::ReferralSplitterEvents::SELECTORS,
        ),
    ];
    for (emitter, selectors) in watched {
        for selector in selectors {
            let topic = B256::from(*selector);
            let log = mined(
                emitter,
                LogData::new_unchecked(vec![topic], Default::default()),
            );
            let skipped = matches!(CONTRACTS.decode(&log), Ok(None));
            let named = CONTRACTS.unread(&log).is_some();
            assert_eq!(
                skipped, named,
                "{emitter} {topic}: skipped by decode {skipped}, named unread {named}"
            );
        }
    }
}

/// The refusal names no libID-contracts release, so a dependency bump cannot
/// leave it pointing operators at an older one.
#[test]
fn an_unread_registry_is_pointed_at_a_node_keyed_registry() {
    let refused = RegistryRefused::Unread {
        registry: REGISTRY,
        source: contract::Error::UnknownFunction("nodeKeyed".into()),
    };
    assert_eq!(
        refused.to_string(),
        format!(
            "registry {REGISTRY}: nodeKeyed() could not be read; point it at a node-keyed libID-contracts IdentityRegistry"
        )
    );
}
