//! Fixtures the usernames integration suites share.
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
//! - [`Seed`]: a chain's store seeded the way its indexer writes it — the
//!   right to write it, and one binding with the chain marked synced.
//! - [`IdNode`]: a user id's node, as the chain keys it.
//! - [`LockWaits`]: a session of the database caught waiting on a lock.
//!
//! `gateway.rs` drives the router in process with these; `end_to_end.rs`
//! deploys the real `HandleResolver` on anvil and walks the protocol with
//! them. Neither needs ENS or a network.
//!
//! The suites that write a chain share the rest:
//!
//! - [`Suite`]: a suite's chain in the database the suites share, written
//!   through the indexer's own window ([`Writes`]) and read through the real
//!   router ([`Reads`]), its answers typed ([`Reply`]).
//! - [`Events`]: the events the contracts emit, as the suites write them.
//! - [`ChainWatch`]: a chain's row, followed through the notices of its
//!   changes.
//! - [`libid`]: the libid contracts deployed on anvil, and the calls a
//!   holder or a payer makes to them.

#![deny(missing_docs)]
#![deny(dead_code)]

pub mod libid;
mod suite;

use std::{
    future::Future,
    time::Duration,
};

use alloy::{
    primitives::{
        Address,
        B256,
        U256,
    },
    sol_types::SolCall,
};
use sqlx::PgPool;
use usernames_core::{
    chain::Contracts,
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

pub use suite::{
    anvil_available,
    database_url,
    ChainWatch,
    Entries,
    Events,
    Reads,
    Reply,
    Suite,
    Writes,
    PATIENCE,
};

/// The registry a chain these fixtures write is prepared for.
const REGISTRY: Address = Address::repeat_byte(0x1D);

/// A user id's node on a platform.
pub trait IdNode {
    /// The node `id` is bound under, as libid-identity derives it.
    fn id_node(self, id: &str) -> B256;
}

impl IdNode for KnownPlatform {
    fn id_node(self, id: &str) -> B256 {
        let spec =
            libid_identity::platforms::platform(self.key()).expect("a libid platform");
        B256::from(spec.id_node(id).expect("an id"))
    }
}

/// The sessions of a database, as the suites watch them.
pub trait LockWaits {
    /// Return once a session of this database named `application` waits on
    /// a lock. Postgres announces no lock wait, so this reads
    /// `pg_stat_activity` every few milliseconds, for at most five seconds.
    fn waits_on_a_lock(&self, application: &str) -> impl Future<Output = ()> + Send;
}

impl LockWaits for PgPool {
    async fn waits_on_a_lock(&self, application: &str) {
        let waiting = async {
            while !sqlx::query_scalar::<_, bool>(include_str!("waiting_on_a_lock.sql"))
                .bind(application)
                .fetch_one(self)
                .await
                .expect("the sessions")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap_or_else(|_| panic!("{application} never waited on a lock"));
    }
}

/// A chain's store, seeded the way its indexer writes it.
pub trait Seed {
    /// The right to write the chain, taken the way an indexer takes it at
    /// startup: under the chain's lease, prepared for one registry. The
    /// lease goes back at once; the writer stays good until the chain is
    /// prepared again.
    fn writer(&self) -> impl Future<Output = ChainWriter> + Send;

    /// One disclosing X binding: its `IdentityBound`, `HandleRevealed` and
    /// `HandlePublished`, in one transaction, with the chain marked synced.
    fn bind(&self, handle: &str, holder: Address) -> impl Future<Output = ()> + Send;

    /// [`Seed::bind`] without the `HandleRevealed` and `HandlePublished`:
    /// an unlisted binding.
    fn bind_private(
        &self,
        handle: &str,
        holder: Address,
    ) -> impl Future<Output = ()> + Send;
}

impl Seed for ChainStore {
    async fn writer(&self) -> ChainWriter {
        let lease = self.acquire_writer().await.expect("the writer lease");
        let writer = self
            .prepare(
                &lease,
                Contracts {
                    registry: REGISTRY,
                    escrow: None,
                },
            )
            .await
            .expect("the chain is prepared");
        lease.release().await.expect("the lease goes back");
        writer
    }

    async fn bind(&self, handle: &str, holder: Address) {
        self.writer().await.seed(handle, holder, true).await;
    }

    async fn bind_private(&self, handle: &str, holder: Address) {
        self.writer().await.seed(handle, holder, false).await;
    }
}

/// A binding written the way the indexer writes one.
trait Binding {
    /// An X binding of `handle` to `holder` at block 1, disclosed or not,
    /// and the chain's head and target there.
    async fn seed(&self, handle: &str, holder: Address, disclosed: bool);
}

impl Binding for ChainWriter {
    async fn seed(&self, handle: &str, holder: Address, disclosed: bool) {
        let x = KnownPlatform::X;
        let handle_node = x.handle_node(handle).expect("an X handle");
        let mut events = vec![NamesEvent::IdentityBound {
            holder,
            id_node: x.id_node("42"),
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
        let mut window = self.begin_window().await.expect("begin");
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
        // The indexer records both every cycle, and the gateway refuses to
        // answer from an index that cannot say how far behind it is.
        // Staleness is measured against the TARGET — the block the cursor
        // chases — so that is the one a fixture must record.
        self.set_chain_head(1).await;
        self.set_chain_target(1, Duration::from_secs(120)).await;
    }
}
