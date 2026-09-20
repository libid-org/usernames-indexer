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
//! - [`wire_name`] and [`namehash_of`]: the name as the wire and the resolver
//!   carry it.
//! - [`addr_call`] and [`legacy_addr_call`]: the record request, in the
//!   ENSIP-11 multichain form and in the older form that means coin type 60.
//! - [`resolve_call`]: the wrapped call the gateway receives.
//! - [`bind`]: one binding written into the store the gateway answers from,
//!   with its chain marked synced.
//!
//! `gateway.rs` drives the router in process with these; `end_to_end.rs`
//! deploys the real `HandleResolver` on anvil and walks the protocol with
//! them. Neither needs ENS or a network. It is a dev-dependency of
//! `usernames-api` and nothing else links it. It is a crate rather than a
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

use alloy::{
    primitives::{
        Address,
        B256,
    },
    sol_types::SolCall,
};
use usernames_core::{
    db::ChainStore,
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes,
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
pub fn namehash_of(labels: &[&str]) -> B256 {
    let full: Vec<String> = labels
        .iter()
        .map(|l| (*l).to_string())
        .chain(["handles".to_string(), "link".to_string()])
        .collect();
    usernames_core::ens::Name::from_labels(full).node()
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
        coinType: alloy::primitives::U256::from(coin),
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

/// Seed one binding into the read model the gateway answers from.
pub async fn bind(store: &ChainStore, handle: &str, owner: Address) {
    let platform = nodes::Platform::from_key("x").unwrap().id();
    let event = NamesEvent::IdentityBound {
        owner,
        id_node: nodes::id_node(platform, "42"),
        handle_node: nodes::handle_node(
            platform,
            &nodes::NormalizedHandle::from_chain(handle),
        ),
        platform_id: platform,
        user_id: "42".into(),
        handle: handle.into(),
        observed_at: 1_700_000_000,
        ceremony_version: 1,
        published: true,
    };
    let mut window = store.begin_window().await.expect("begin");
    window
        .apply(
            &event,
            &LogPosition {
                block_number: 1,
                log_index: 0,
                tx_hash: B256::from([1u8; 32]),
            },
        )
        .await
        .expect("apply");
    window.commit(1).await.expect("commit");
    // The indexer records both every cycle, and the gateway refuses to answer
    // from an index that cannot say how far behind it is. Staleness is measured
    // against the TARGET — the block the cursor chases — so that is the one a
    // fixture must record.
    store.set_chain_head(1).await;
    store.set_chain_target(1, 120).await;
}
