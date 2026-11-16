//! Event decoding: each contract log as the journal records it.

use alloy::{
    primitives::{
        Address,
        B256,
        U256,
    },
    rpc::types::Log,
    sol_types::SolEvent,
};
use libid_contracts::bindings::{
    escrow::HandleEscrow,
    identity::{
        IdentityRegistry,
        IdentityRegistry::HandleRevealed,
    },
    referral::ReferralSplitter,
};
use usernames_core::nodes::DisclosedHandle;

use alloy::{
    primitives,
    primitives::{
        b256,
        LogData,
    },
};

use usernames_core::{
    events::*,
    nodes::KnownPlatform,
};

const HOLDER: Address = Address::repeat_byte(0xA1);

/// A log carrying `data`, as the RPC returns it once mined.
fn mined(data: LogData) -> Log {
    Log {
        inner: primitives::Log {
            address: Address::repeat_byte(0x11),
            data,
        },
        block_number: Some(7),
        log_index: Some(3),
        transaction_hash: Some(B256::repeat_byte(0x22)),
        block_timestamp: Some(1_700_000_000),
        ..Log::default()
    }
}

/// Fixed points from `cast keccak` over the event signatures in
/// `IdentityRegistry.sol`, so a crate binding that drifts from the contract
/// fails against numbers this code never produced.
#[test]
fn topics_match_the_contract() {
    assert_eq!(
        IdentityRegistry::IdentityBound::SIGNATURE_HASH,
        b256!("7de04595ed86c830556fbd7a7b60045145ee35b154a004b0c530ef91a4bd836c")
    );
    assert_eq!(
        IdentityRegistry::HandlePublished::SIGNATURE_HASH,
        b256!("04a31fa5777bb2c9381e5ebd6b6f967c2094d3874b1c7789dc15a80c7dcbcafb")
    );
    assert_eq!(
        HandleRevealed::SIGNATURE_HASH,
        b256!("8ba0ebc5838d01aae10591d451b810b02cbd938a799162a7dda37f1d37b5e9f7")
    );
    assert_eq!(
        IdentityRegistry::HandleRetired::SIGNATURE_HASH,
        b256!("278e2122d24782b703f05416ac53a9750d67bcf6fa1603b94ec95660821a8914")
    );
    assert_eq!(
        IdentityRegistry::CeremonyBound::SIGNATURE_HASH,
        b256!("f0f0b831e902ded46acfd6caf87649edb445c2e2733992b2e79cd1420719c19c")
    );
    assert_eq!(
        IdentityRegistry::BindFeePaid::SIGNATURE_HASH,
        b256!("471f5d0665e5719861746aa9077ef4997ae3d84cad4f349f5e7cc2cfe74be599")
    );
    assert_eq!(
        IdentityRegistry::ProofVerifierConfigured::SIGNATURE_HASH,
        b256!("01970f3cbef68f8ac95615ab5c75299e421a83a787173408d693928ed40b6574")
    );
    assert_eq!(
        IdentityRegistry::HandleUnpublished::SIGNATURE_HASH,
        b256!("916ce5f64b56cb8a45184ccbfe46b3eb4a38158e9cb0defe417c42a9bc42fda5")
    );
}

#[test]
fn handle_unpublished_decodes() {
    let log =
        mined(IdentityRegistry::HandleUnpublished { holder: HOLDER }.encode_log_data());
    let (event, position) = NamesEvent::from_registry(&log)
        .unwrap()
        .expect("a known topic");
    assert!(
        matches!(
            event,
            NamesEvent::HandleUnpublished { holder } if holder == HOLDER
        ),
        "{event:?}"
    );
    assert_eq!(event.kind(), EventKind::HandleUnpublished);
    assert_eq!((position.block_number, position.log_index), (7, 3));
    assert_eq!(position.block_time, 1_700_000_000);
}

/// The same fixed points for `HandleEscrow.sol`'s own four events, and
/// the `HandleRevealed` it shares with the registry.
#[test]
fn escrow_topics_match_the_contract() {
    assert_eq!(
        HandleEscrow::HandleRevealed::SIGNATURE_HASH,
        HandleRevealed::SIGNATURE_HASH
    );
    assert_eq!(
        HandleEscrow::Deposited::SIGNATURE_HASH,
        b256!("590730108e214366abd2912d060c6195480c59b924619145601d0d07ab14a61d")
    );
    assert_eq!(
        HandleEscrow::Forwarded::SIGNATURE_HASH,
        b256!("afdf4d834a7c59206d8f48e1bb08e8625d11809b936b1eea92c3333534bdf7c3")
    );
    assert_eq!(
        HandleEscrow::Claimed::SIGNATURE_HASH,
        b256!("1250cc5278315bb2cb7738cc5a09976c58b02915c0276c970e7f29516954cce6")
    );
    assert_eq!(
        HandleEscrow::Refunded::SIGNATURE_HASH,
        b256!("b7d023fdcfc8cdb1aeb0fcf141fb12597f076f2d9cfa676aba35f34489d52e4e")
    );
}

#[test]
fn a_deposit_decodes_with_its_sender_and_refund_date() {
    let node = B256::repeat_byte(0x0D);
    let token = Address::repeat_byte(0xEE);
    let payer = Address::repeat_byte(0xB0);
    let log = mined(
        HandleEscrow::Deposited {
            handleNode: node,
            token,
            sender: payer,
            round: U256::from(2u64),
            amount: U256::MAX,
            refundableAt: u64::MAX,
        }
        .encode_log_data(),
    );
    let (event, _) = NamesEvent::from_escrow(&log)
        .unwrap()
        .expect("a known topic");
    assert_eq!(event.kind(), EventKind::Deposited);
    assert_eq!(event.parties(), [(payer, Role::Sender)]);
    assert!(matches!(
        event,
        NamesEvent::Escrow(EscrowEvent::Deposited { refundable_at, .. })
            if refundable_at == u64::MAX
    ));
}

/// An escrow topic means nothing from the registry, and the reverse: each
/// contract's logs are decoded against its own events, and the
/// `HandleRevealed` they share.
#[test]
fn each_decoder_knows_only_its_own_contract() {
    let escrow_log = mined(
        HandleEscrow::Refunded {
            handleNode: B256::ZERO,
            token: Address::ZERO,
            sender: HOLDER,
            recipient: HOLDER,
            round: U256::ZERO,
            released: U256::from(1u64),
            received: U256::from(1u64),
        }
        .encode_log_data(),
    );
    assert!(NamesEvent::from_registry(&escrow_log).unwrap().is_none());
    let registry_log =
        mined(IdentityRegistry::HandleUnpublished { holder: HOLDER }.encode_log_data());
    assert!(NamesEvent::from_escrow(&registry_log).unwrap().is_none());
}

#[test]
fn a_log_without_its_block_time_fails_instead_of_dating_nothing() {
    let mut log =
        mined(IdentityRegistry::HandleUnpublished { holder: HOLDER }.encode_log_data());
    log.block_timestamp = None;
    assert!(matches!(
        NamesEvent::from_registry(&log),
        Err(DecodeError::MissingField("block timestamp"))
    ));
}

/// Revealed text decodes only when it hashes to the node beside it.
#[test]
fn a_revealed_handle_keeps_its_text_only_when_it_hashes_to_the_node() {
    let x = KnownPlatform::X;
    let node = x.handle_node("alice_1").unwrap();
    let revealed = |handle: &str| {
        let log = mined(
            HandleRevealed {
                platformId: x.id(),
                handleNode: node,
                handle: handle.into(),
            }
            .encode_log_data(),
        );
        match NamesEvent::from_registry(&log)
            .unwrap()
            .expect("a known topic")
            .0
        {
            NamesEvent::HandleRevealed { handle, .. } => handle,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        revealed("Alice_1").as_ref().map(DisclosedHandle::as_str),
        Some("alice_1")
    );
    assert_eq!(revealed("bob"), None);
}

/// The escrow's `HandleRevealed` is the registry's: the same event, which
/// names no address.
#[test]
fn an_escrow_reveal_decodes_as_the_registrys() {
    let x = KnownPlatform::X;
    let node = x.handle_node("bob_1").unwrap();
    let log = mined(
        HandleEscrow::HandleRevealed {
            platformId: x.id(),
            handleNode: node,
            handle: "Bob_1".into(),
        }
        .encode_log_data(),
    );
    let (from_escrow, _) = NamesEvent::from_escrow(&log)
        .unwrap()
        .expect("a known topic");
    let (from_registry, _) = NamesEvent::from_registry(&log)
        .unwrap()
        .expect("a known topic");
    assert_eq!(from_escrow, from_registry);
    assert_eq!(from_escrow.parties(), []);
    assert_eq!(
        from_escrow
            .disclosure()
            .map(|(_, n, text)| (n, text.as_str())),
        Some((node, "bob_1"))
    );
}

/// A `Referred` log, outcome as the splitter's enum carries it.
fn referred(outcome: u8) -> Log {
    mined(
        ReferralSplitter::Referred {
            referrerNode: B256::repeat_byte(0x0A),
            binder: HOLDER,
            sessionId: B256::repeat_byte(0x5E),
            referrerPlatformId: KnownPlatform::X.id(),
            platformId: KnownPlatform::GitHub.id(),
            handleNode: B256::repeat_byte(0x0B),
            fee: U256::MAX,
            share: U256::from(3u64),
            outcome,
        }
        .encode_log_data(),
    )
}

/// The fixed point from `cast keccak` over the event signature in
/// `ReferralSplitter.sol`, its enum as `uint8`.
#[test]
fn splitter_topic_matches_the_contract() {
    assert_eq!(
        ReferralSplitter::Referred::SIGNATURE_HASH,
        b256!("5a7143a89e5ba1b9f91f71af11ac6db2ff16743bc25391994d13a8866478f47a")
    );
    assert_eq!(
        ReferralSplitter::FeeSet::SIGNATURE_HASH,
        b256!("20461e09b8e557b77e107939f9ce6544698123aad0fc964ac5cc59b7df2e608f")
    );
}

#[test]
fn a_fee_set_decodes_as_the_bind_fee() {
    let log = mined(
        ReferralSplitter::FeeSet {
            fee: U256::from(7u64),
        }
        .encode_log_data(),
    );
    let (event, _) = NamesEvent::from_splitter(&log)
        .unwrap()
        .expect("a known topic");
    assert_eq!(
        event,
        NamesEvent::BindFeeSet {
            fee: U256::from(7u64)
        }
    );
    assert_eq!(event.kind(), EventKind::BindFeeSet);
    assert!(event.parties().is_empty());
    assert_eq!(event.handle_node(), None);
}

#[test]
fn a_referral_decodes_with_its_outcome_and_binder() {
    let (event, _) = NamesEvent::from_splitter(&referred(1))
        .unwrap()
        .expect("a known topic");
    assert_eq!(
        event,
        NamesEvent::Referred {
            referrer_node: B256::repeat_byte(0x0A),
            binder: HOLDER,
            session_id: B256::repeat_byte(0x5E),
            referrer_platform_id: KnownPlatform::X.id(),
            platform_id: KnownPlatform::GitHub.id(),
            handle_node: B256::repeat_byte(0x0B),
            fee: U256::MAX,
            share: U256::from(3u64),
            outcome: Outcome::SelfReferral,
        }
    );
    assert_eq!(event.kind(), EventKind::Referred);
    assert_eq!(event.parties(), [(HOLDER, Role::Holder)]);
    assert_eq!(event.handle_node(), Some(B256::repeat_byte(0x0B)));
}

#[test]
fn an_outcome_past_the_splitters_enum_fails_the_decode() {
    assert!(matches!(
        NamesEvent::from_splitter(&referred(3)),
        Err(DecodeError::Outcome(3))
    ));
}

/// The splitter's logs are read only as the splitter's, and its decoder
/// reads neither the registry's nor the escrow's.
#[test]
fn the_splitter_decoder_knows_only_its_own_contract() {
    assert!(NamesEvent::from_registry(&referred(0)).unwrap().is_none());
    assert!(NamesEvent::from_escrow(&referred(0)).unwrap().is_none());
    let registry_log =
        mined(IdentityRegistry::HandleUnpublished { holder: HOLDER }.encode_log_data());
    assert!(NamesEvent::from_splitter(&registry_log).unwrap().is_none());
}

#[test]
fn every_outcome_and_delivery_parses_back_from_its_name() {
    for outcome in Outcome::ALL {
        assert_eq!(outcome.as_str().parse::<Outcome>().ok(), Some(outcome));
    }
    for delivery in Delivery::ALL {
        assert_eq!(delivery.as_str().parse::<Delivery>().ok(), Some(delivery));
    }
    assert!("pending".parse::<Outcome>().is_err());
    assert!("lost".parse::<Delivery>().is_err());
}

/// What a chain history's `?kind=` takes, the splitter's kinds included.
#[test]
fn every_kind_parses_back_from_its_name() {
    for kind in EventKind::ALL {
        assert_eq!(kind.as_str().parse::<EventKind>().ok(), Some(kind));
    }
    assert_eq!(
        ["referred", "bind_fee_set"].map(|name| name.parse::<EventKind>().ok()),
        [Some(EventKind::Referred), Some(EventKind::BindFeeSet)]
    );
    assert!("minted".parse::<EventKind>().is_err());
}

#[test]
fn every_role_parses_back_from_its_name() {
    for role in Role::ALL {
        assert_eq!(role.as_str().parse::<Role>().ok(), Some(role));
    }
    assert!("owner".parse::<Role>().is_err());
}
