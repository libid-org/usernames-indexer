//! Request fixtures for the `usernames-api` integration suites.
//!
//! A wallet resolving `alice.x.handles.link` through ENS does not send that
//! text. It sends the name in DNS wire form and its namehash, plus an
//! ABI-encoded request for the `addr` record, wrapped in the ENSIP-10
//! `resolve(name, data)` call that the resolver forwards to the gateway. The
//! suites in `bin/usernames-api/tests` send hundreds of such requests, so
//! this crate holds one builder per piece, and every suite sends the bytes a
//! wallet would:
//!
//! - [`wire_name`]: the name as the wire carries it.
//! - [`addr_call`] and [`legacy_addr_call`]: the record request, in the
//!   ENSIP-11 multichain form and in the older form that means coin type 60.
//! - [`resolve_call`]: the wrapped call the gateway receives.
//! - [`bind`] and [`bind_private`]: one binding written into the store the
//!   gateway answers from, with its chain marked synced.
//! - [`writer`]: the right to write a chain, as an indexer takes it.
//!
//! `gateway.rs` drives the router in process with these; `end_to_end.rs`
//! deploys the real `HandleResolver` on anvil and walks the protocol with
//! them. Neither needs ENS or a network. It is a crate rather than a
//! `tests/common` module because each test binary compiles such a module on
//! its own, and a helper one of them does not call reads as dead code there;
//! a library's public items never do.
//!
//! To test against a real wallet instead of these suites, the resolver ENS
//! holds for `handles.link` must name your gateway. On Sepolia that resolver
//! is a `HandleResolver` whose URL is
//! `http://127.0.0.1:8080/ens/{sender}/{data}.json`: a `usernames-api`
//! running locally, with a signer the resolver trusts (`setSigner`), answers
//! it. `setUrls` repoints it; both are calls by the resolver's owner.

#![deny(missing_docs)]
#![deny(dead_code)]

use std::time::Duration;

use alloy::{
    primitives::{
        Address,
        B256,
        U256,
    },
    sol_types::SolCall,
};
use usernames_core::{
    db::{
        ChainStore,
        ChainWriter,
    },
    ens::Name,
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes::{
        DisclosedHandle,
        KnownPlatform,
    },
};

mod calls {
    use alloy::sol;

    sol! {
        /// ENSIP-10, as the resolver forwards it.
        function resolve(bytes name, bytes data) external view returns (bytes);
        /// ENSIP-11's multichain form.
        function addr(bytes32 node, uint256 coinType) external view returns (bytes);
        /// The pre-ENSIP-11 form, which implies coin type 60.
        function addr(bytes32 node) external view returns (address);
    }
}
use calls::{
    addr_0Call,
    addr_1Call,
    resolveCall,
};

/// `alice.x` becomes `alice.x.handles.link`, in DNS wire format.
pub fn wire_name(labels: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for label in labels.iter().chain(["handles", "link"].iter()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// The namehash of the same name, as a wallet sends it beside the wire form.
fn namehash_of(labels: &[&str]) -> B256 {
    Name::parse(&wire_name(labels))
        .expect("a name in wire format")
        .node()
}

/// `resolve(name, data)`, encoded by the same codec the gateway decodes with.
pub fn resolve_call(name: &[u8], inner: &[u8]) -> Vec<u8> {
    resolveCall {
        name: name.to_vec().into(),
        data: inner.to_vec().into(),
    }
    .abi_encode()
}

/// `addr(node, coinType)` for the name the labels spell.
pub fn addr_call(labels: &[&str], coin: u64) -> Vec<u8> {
    addr_0Call {
        node: namehash_of(labels),
        coinType: U256::from(coin),
    }
    .abi_encode()
}

/// `addr(node)` — the legacy shape, which means coin type 60.
pub fn legacy_addr_call(labels: &[&str]) -> Vec<u8> {
    addr_1Call {
        node: namehash_of(labels),
    }
    .abi_encode()
}

/// Seed one disclosing X binding into the read model the gateway answers
/// from: its `IdentityBound`, `HandleRevealed` and `HandlePublished`, in one
/// transaction.
pub async fn bind(store: &ChainStore, handle: &str, holder: Address) {
    seed(store, handle, holder, true).await;
}

/// The registry a chain these fixtures write is prepared for.
const REGISTRY: Address = Address::repeat_byte(0x1D);

/// The right to write `store`'s chain, taken the way an indexer takes it at
/// startup: under the chain's lease, prepared for `REGISTRY`. The lease
/// goes back at once; the writer stays good until the chain is prepared
/// again.
pub async fn writer(store: &ChainStore) -> ChainWriter {
    let lease = store.acquire_writer().await.expect("the writer lease");
    let writer = store
        .prepare(&lease, REGISTRY, None)
        .await
        .expect("the chain is prepared");
    lease.release().await.expect("the lease goes back");
    writer
}

/// [`bind`] without the `HandleRevealed` and `HandlePublished`: a private
/// binding.
pub async fn bind_private(store: &ChainStore, handle: &str, holder: Address) {
    seed(store, handle, holder, false).await;
}

async fn seed(store: &ChainStore, handle: &str, holder: Address, disclosed: bool) {
    let x = KnownPlatform::X;
    let handle_node = x.handle_node(handle).expect("an X handle");
    let mut events = vec![NamesEvent::IdentityBound {
        holder,
        id_node: B256::from(
            libid_identity::platforms::platform(x.key())
                .and_then(|spec| spec.id_node("42").ok())
                .expect("an X id"),
        ),
        handle_node,
        platform_id: x.id(),
        observed_at: 1_700_000_000,
        ceremony_version: 1,
    }];
    if disclosed {
        events.push(NamesEvent::HandleRevealed {
            platform_id: x.id(),
            handle_node,
            handle: DisclosedHandle::verify(x, handle_node, handle),
        });
        events.push(NamesEvent::HandlePublished {
            holder,
            platform_id: x.id(),
            handle_node,
        });
    }
    let writer = writer(store).await;
    let mut window = writer.begin_window().await.expect("begin");
    for (log_index, event) in (0u64..).zip(&events) {
        window
            .apply(
                event,
                &LogPosition {
                    block_number: 1,
                    log_index,
                    tx_hash: B256::from([1u8; 32]),
                    block_time: 1_700_000_000,
                },
            )
            .await
            .expect("apply");
    }
    window.commit(1).await.expect("commit");
    // The indexer records both every cycle, and the gateway refuses to answer
    // from an index that cannot say how far behind it is. Staleness is measured
    // against the TARGET — the block the cursor chases — so that is the one a
    // fixture must record.
    writer.set_chain_head(1).await;
    writer.set_chain_target(1, Duration::from_secs(120)).await;
}
