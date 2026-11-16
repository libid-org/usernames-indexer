//! The journal: how event rows are written and read back.

use alloy::primitives::{
    Address,
    Bytes,
    B256,
    U256,
};
use usernames_core::events::{
    EscrowEvent,
    EventKind,
    NamesEvent,
};

use usernames_core::{
    db::journal::*,
    events::Outcome,
    nodes::{
        DisclosedHandle,
        KnownPlatform,
    },
};

/// Every kind, the largest `uint256` and counts included, reads back from
/// the columns it writes: all of it but disclosed text, which the journal
/// never holds.
#[test]
fn every_event_reads_back_from_its_columns() {
    let (a, b) = (Address::repeat_byte(0xA1), Address::repeat_byte(0xB2));
    let (n, p) = (B256::repeat_byte(0x0D), B256::repeat_byte(0x0C));
    let big = U256::MAX - U256::from(1u64);
    let x = KnownPlatform::X;
    let alice = x.handle_node("alice").unwrap();
    let text = DisclosedHandle::verify(x, alice, "Alice");
    assert!(text.is_some());
    let events = [
        (
            NamesEvent::IdentityBound {
                holder: a,
                id_node: n,
                handle_node: p,
                platform_id: x.id(),
                observed_at: i64::MAX as u64,
                ceremony_version: u16::MAX,
            },
            Some(p),
        ),
        (
            NamesEvent::HandleRevealed {
                platform_id: x.id(),
                handle_node: alice,
                handle: text,
            },
            Some(alice),
        ),
        (
            NamesEvent::HandlePublished {
                holder: a,
                platform_id: x.id(),
                handle_node: alice,
            },
            Some(alice),
        ),
        (
            NamesEvent::HandleRetired {
                platform_id: x.id(),
                handle_node: n,
                holder: a,
            },
            Some(n),
        ),
        (NamesEvent::HandleUnpublished { holder: a }, Some(alice)),
        (NamesEvent::ProofVerifierConfigured { verifier: b }, None),
        (
            NamesEvent::CeremonyBound {
                authorization_digest: p,
                holder: a,
                platform_id: x.id(),
                client_identifier: Bytes::from_static(b"client"),
            },
            Some(n),
        ),
        (
            NamesEvent::BindFeePaid {
                authorization_digest: p,
                receiver: b,
                amount: big,
            },
            Some(n),
        ),
        (
            NamesEvent::Escrow(EscrowEvent::Deposited {
                handle_node: n,
                token: b,
                sender: a,
                round: U256::from(2u64),
                amount: U256::MAX,
                refundable_at: i64::MAX as u64,
            }),
            Some(n),
        ),
        (
            NamesEvent::Escrow(EscrowEvent::Forwarded {
                handle_node: n,
                token: b,
                sender: a,
                holder: b,
                amount: big,
                received: big,
            }),
            Some(n),
        ),
        (
            NamesEvent::Escrow(EscrowEvent::Claimed {
                handle_node: n,
                token: b,
                claimer: a,
                recipient: b,
                round: U256::ZERO,
                released: big,
                received: U256::from(1u64),
            }),
            Some(n),
        ),
        (
            NamesEvent::Escrow(EscrowEvent::Refunded {
                handle_node: n,
                token: b,
                sender: a,
                recipient: b,
                round: U256::from(1u64),
                released: U256::from(5u64),
                received: U256::from(4u64),
            }),
            Some(n),
        ),
        (
            NamesEvent::Referred {
                referrer_node: n,
                binder: a,
                session_id: p,
                referrer_platform_id: x.id(),
                platform_id: KnownPlatform::Google.id(),
                handle_node: alice,
                fee: big,
                share: U256::MAX,
                outcome: Outcome::SelfReferral,
            },
            Some(alice),
        ),
        (NamesEvent::BindFeeSet { fee: U256::MAX }, None),
    ];
    for (event, handle_node) in events {
        let row = EventColumns::of(&event, handle_node).unwrap();
        assert_eq!(row.handle_node, handle_node);
        let read = NamesEvent::try_from(&row)
            .unwrap_or_else(|e| panic!("{}: {e}", event.kind()));
        assert_eq!(read, event.without_text());
    }
}

/// A column the kind carries, left NULL, is refused by name.
#[test]
fn a_row_missing_a_column_is_refused_by_name() {
    let row = EventColumns {
        kind: EventKind::HandleUnpublished.to_string(),
        ..EventColumns::default()
    };
    assert!(matches!(
        NamesEvent::try_from(&row),
        Err(JournalError::Missing {
            field: "holder",
            ..
        })
    ));
}
