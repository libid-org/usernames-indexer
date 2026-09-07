//! Fixtures both integration suites build requests from.
//!
//! One copy on purpose. These were written twice, and the pair drifted the
//! first time the read model gained a field a fixture had to record: the fix
//! had to find every copy. Anything a request must look like belongs here, so
//! the next such change is one edit.

// Each integration binary compiles this module separately, so whatever the one
// being built does not call reads as dead. The alternative is splitting the
// fixtures by which suite happens to use them, which is how they drifted apart
// in the first place.
#![allow(dead_code)]

use alloy::{
    primitives::{
        Address,
        B256,
    },
    sol,
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

sol! {
    /// ENSIP-10, as the resolver forwards it.
    function resolve(bytes name, bytes data) external view returns (bytes);
    /// ENSIP-11's multichain form.
    function addr(bytes32 node, uint256 coinType) external view returns (bytes);
    /// The pre-ENSIP-11 form, which implies coin type 60.
    function addr(bytes32 node) external view returns (address);
}

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
    usernames_core::ens::namehash(&full)
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
        version: 1,
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
    // from a mirror that cannot say how far behind it is. Staleness is measured
    // against the TARGET — the block the cursor chases — so that is the one a
    // fixture must record.
    store.set_chain_head(1).await;
    store.set_chain_target(1, 120).await;
}
