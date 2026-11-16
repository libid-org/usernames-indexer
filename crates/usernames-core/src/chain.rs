//! The contracts an indexer follows, and what it reads of them from the
//! chain: their checks at startup, the registry's deployment block, and
//! their logs window by window.

use std::collections::{
    BTreeSet,
    HashMap,
};

use alloy::{
    consensus::BlockHeader,
    contract,
    eips::BlockNumberOrTag,
    network::BlockResponse,
    primitives::{
        address,
        Address,
        B256,
    },
    providers::Provider,
    rpc::types::{
        Filter,
        Log,
    },
    sol_types::SolEvent,
    transports::{
        TransportError,
        TransportErrorKind,
    },
};
use futures_util::{
    stream,
    StreamExt,
    TryStreamExt,
};
use libid_contracts::bindings::{
    escrow::HandleEscrow,
    identity::IdentityRegistry,
    referral::ReferralSplitter,
};

use crate::events::{
    DecodeError,
    LogPosition,
    NamesEvent,
};

/// How many block headers a window asks for at once, when its endpoint
/// leaves the block times out of the logs.
const HEADERS_AT_ONCE: usize = 8;

/// The address `HandleEscrow` names the chain's own coin by (EIP-7528).
pub const NATIVE_COIN: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

/// The libid contracts one indexer follows on its chain: the registry, and
/// the escrow and the referral splitter when it watches them. The registry
/// and the escrow are ERC1967 proxies, the addresses that emit; the
/// implementation behind each changes on upgrade. The splitter is no proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contracts {
    /// The IdentityRegistry.
    pub registry: Address,
    /// The HandleEscrow, when the indexer watches one.
    pub escrow: Option<Address>,
    /// The ReferralSplitter, when the indexer watches one. It deposits
    /// through the escrow, so it is watched only beside one.
    pub referral_splitter: Option<Address>,
}

/// A contract an indexer watches, by the part it plays.
#[derive(Clone, Copy)]
enum Watched {
    Registry,
    Escrow,
    Splitter,
}

/// An event a watched contract declares and the index does not read.
struct Unread {
    topic: B256,
    signature: &'static str,
}

impl Unread {
    const fn of<E: SolEvent>() -> Self {
        Self {
            topic: E::SIGNATURE_HASH,
            signature: E::SIGNATURE,
        }
    }
}

/// The registry's proxy and ownership events.
const REGISTRY_UNREAD: &[Unread] = &[
    Unread::of::<IdentityRegistry::Initialized>(),
    Unread::of::<IdentityRegistry::OwnershipTransferStarted>(),
    Unread::of::<IdentityRegistry::OwnershipTransferred>(),
    Unread::of::<IdentityRegistry::Upgraded>(),
];

/// The escrow's proxy and ownership events.
const ESCROW_UNREAD: &[Unread] = &[
    Unread::of::<HandleEscrow::Initialized>(),
    Unread::of::<HandleEscrow::OwnershipTransferStarted>(),
    Unread::of::<HandleEscrow::OwnershipTransferred>(),
    Unread::of::<HandleEscrow::Upgraded>(),
];

/// The splitter's ownership, share and treasury events: a referral's own
/// event carries the share and the treasury it paid.
const SPLITTER_UNREAD: &[Unread] = &[
    Unread::of::<ReferralSplitter::OwnershipTransferStarted>(),
    Unread::of::<ReferralSplitter::OwnershipTransferred>(),
    Unread::of::<ReferralSplitter::ShareSet>(),
    Unread::of::<ReferralSplitter::TreasurySet>(),
];

impl Watched {
    /// The events this contract declares that the index does not read.
    fn unread(self) -> &'static [Unread] {
        match self {
            Self::Registry => REGISTRY_UNREAD,
            Self::Escrow => ESCROW_UNREAD,
            Self::Splitter => SPLITTER_UNREAD,
        }
    }
}

impl Contracts {
    /// Every contract whose logs the indexer reads.
    pub fn watched(&self) -> Vec<Address> {
        std::iter::once(self.registry)
            .chain(self.escrow)
            .chain(self.referral_splitter)
            .collect()
    }

    /// Which watched contract `emitter` is.
    fn watched_as(&self, emitter: Address) -> Option<Watched> {
        if emitter == self.registry {
            Some(Watched::Registry)
        } else if Some(emitter) == self.escrow {
            Some(Watched::Escrow)
        } else if Some(emitter) == self.referral_splitter {
            Some(Watched::Splitter)
        } else {
            None
        }
    }

    /// Decode a log by the contract that emitted it: each contract's topics
    /// mean something only from that contract. The filter admits no other
    /// emitter, so one would be an RPC answering a different question.
    pub fn decode(
        &self,
        log: &Log,
    ) -> Result<Option<(NamesEvent, LogPosition)>, DecodeError> {
        match self.watched_as(log.address()) {
            Some(Watched::Registry) => NamesEvent::from_registry(log),
            Some(Watched::Escrow) => NamesEvent::from_escrow(log),
            Some(Watched::Splitter) => NamesEvent::from_splitter(log),
            None => {
                tracing::warn!(
                    emitter = %log.address(),
                    "a log from a contract the filter does not name; skipped"
                );
                Ok(None)
            }
        }
    }

    /// The signature of `log`'s event when it is one its emitter declares
    /// and the index does not read, for a log [`Self::decode`] skips. `None`
    /// is any other topic: a contract changed under the indexer, or an event
    /// the bindings carry that the index does not read yet.
    pub fn unread(&self, log: &Log) -> Option<&'static str> {
        let topic0 = log.topic0()?;
        self.watched_as(log.address())?
            .unread()
            .iter()
            .find(|event| event.topic == *topic0)
            .map(|event| event.signature)
    }

    /// Binary search for the block at which the registry was deployed: the
    /// first block where `eth_getCode` returns non-empty bytecode. `Ok(None)`
    /// means the contract has no code at `latest_block` — absence is an
    /// answer. An RPC failure is an `Err`, kept apart so a transient flake
    /// cannot masquerade as "not deployed" and end up cached as if it were a
    /// fact.
    ///
    /// Cost: about log2(latest_block) RPC calls (~26 for 50M blocks).
    pub async fn registry_deployment_block(
        &self,
        provider: &impl Provider,
        latest_block: u64,
    ) -> Result<Option<u64>, TransportError> {
        let address = self.registry;
        let code = provider
            .get_code_at(address)
            .block_id(BlockNumberOrTag::Number(latest_block).into())
            .await?;
        if code.is_empty() {
            return Ok(None);
        }

        let code_at_zero = provider
            .get_code_at(address)
            .block_id(BlockNumberOrTag::Number(0).into())
            .await?;
        if !code_at_zero.is_empty() {
            return Ok(Some(0));
        }

        let mut lo: u64 = 0;
        let mut hi: u64 = latest_block;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let code = provider
                .get_code_at(address)
                .block_id(BlockNumberOrTag::Number(mid).into())
                .await?;
            if code.is_empty() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(Some(lo))
    }

    /// The watched contracts' logs in one `eth_getLogs` window, inclusive
    /// on both ends, each with its block's time. The caller sizes windows
    /// with its `max_block_range`; a failure fails the window so the cursor
    /// stays put and the same blocks are retried next cycle.
    ///
    /// `blockTimestamp` is not part of `eth_getLogs`'s standard answer, and
    /// an endpoint that omits it is asked for the block's header instead:
    /// once per block, by hash so the time is the logged block's, a few
    /// blocks at a time.
    pub async fn logs(
        &self,
        provider: &impl Provider,
        from: u64,
        to: u64,
    ) -> Result<Vec<Log>, TransportError> {
        let filter = Filter::new()
            .address(self.watched())
            .from_block(BlockNumberOrTag::Number(from))
            .to_block(BlockNumberOrTag::Number(to));
        let mut logs = provider.get_logs(&filter).await?;
        let untimed: BTreeSet<B256> = logs
            .iter()
            .filter(|log| log.block_timestamp.is_none())
            .filter_map(|log| log.block_hash)
            .collect();
        let times: HashMap<B256, u64> = stream::iter(untimed)
            .map(|hash| async move {
                let block = provider.get_block_by_hash(hash).await?.ok_or_else(|| {
                    TransportErrorKind::custom_str(&format!(
                        "no block {hash} for the logs logged in it"
                    ))
                })?;
                Ok::<_, TransportError>((hash, block.header().timestamp()))
            })
            .buffer_unordered(HEADERS_AT_ONCE)
            .try_collect()
            .await?;
        for log in &mut logs {
            if log.block_timestamp.is_none() {
                log.block_timestamp =
                    log.block_hash.and_then(|hash| times.get(&hash).copied());
            }
        }
        Ok(logs)
    }

    /// Refuse a registry whose events this build would misread: one not
    /// keyed by circuit nodes. Checked once, at startup, before a row is
    /// touched.
    pub async fn check_registry(
        &self,
        provider: &impl Provider,
    ) -> Result<(), RegistryRefused> {
        let registry = self.registry;
        let keyed = IdentityRegistry::new(registry, provider)
            .nodeKeyed()
            .call()
            .await
            .map_err(|source| RegistryRefused::Unread { registry, source })?;
        if keyed {
            Ok(())
        } else {
            Err(RegistryRefused::NotNodeKeyed { registry })
        }
    }

    /// Refuse an escrow that does not resolve holders through the registry;
    /// without an escrow there is nothing to refuse. An escrow keeps its
    /// registry for life, so an indexer checks once, at startup, before it
    /// touches a row.
    pub async fn check_escrow(
        &self,
        provider: &impl Provider,
    ) -> Result<(), EscrowRefused> {
        let Some(escrow) = self.escrow else {
            return Ok(());
        };
        let resolves = HandleEscrow::new(escrow, provider)
            .registry()
            .call()
            .await
            .map_err(|source| EscrowRefused::Unread { escrow, source })?;
        if resolves == self.registry {
            Ok(())
        } else {
            Err(EscrowRefused::OtherRegistry {
                escrow,
                registry: resolves,
                indexed: self.registry,
            })
        }
    }

    /// Refuse a splitter that is not wired to the registry and the escrow
    /// this indexer follows; without a splitter there is nothing to refuse.
    /// A paid referral is read from the escrow's event before it, so a
    /// splitter that pays through another escrow, or with none watched,
    /// would leave every paid referral unpaired. Both addresses are
    /// immutable in the splitter, so an indexer checks once, at startup.
    pub async fn check_referrals(
        &self,
        provider: &impl Provider,
    ) -> Result<(), ReferralsRefused> {
        let Some(splitter) = self.referral_splitter else {
            return Ok(());
        };
        let Some(escrow) = self.escrow else {
            return Err(ReferralsRefused::NoEscrow { splitter });
        };
        let instance = ReferralSplitter::new(splitter, provider);
        let unread = |source| ReferralsRefused::Unread { splitter, source };
        let (registry, escrow_call) = (instance.REGISTRY(), instance.ESCROW());
        let (registry, pays_through) =
            tokio::try_join!(registry.call(), escrow_call.call()).map_err(unread)?;
        if registry != self.registry || pays_through != escrow {
            return Err(ReferralsRefused::OtherContracts {
                splitter,
                registry,
                escrow: pays_through,
            });
        }
        Ok(())
    }
}

/// Why a referral splitter is refused beside the contracts an indexer follows.
#[derive(Debug, thiserror::Error)]
pub enum ReferralsRefused {
    /// A splitter configured with no escrow to read its deposits from.
    #[error(
        "splitter {splitter} needs HANDLE_ESCROW_ADDRESS: it pays through the escrow"
    )]
    NoEscrow {
        /// The splitter configured.
        splitter: Address,
    },
    /// The splitter's `REGISTRY()` or `ESCROW()` could not be read.
    #[error("splitter {splitter}: REGISTRY() or ESCROW() could not be read")]
    Unread {
        /// The splitter asked.
        splitter: Address,
        /// The call's failure.
        #[source]
        source: contract::Error,
    },
    /// The splitter is paid by another registry or pays through another
    /// escrow.
    #[error(
        "splitter {splitter} serves registry {registry} through escrow {escrow}, \
         not the ones indexed"
    )]
    OtherContracts {
        /// The splitter asked.
        splitter: Address,
        /// The registry it takes fees from.
        registry: Address,
        /// The escrow it pays through.
        escrow: Address,
    },
}

/// Why a registry is refused.
#[derive(Debug, thiserror::Error)]
pub enum RegistryRefused {
    /// `nodeKeyed()` could not be read: not a node-keyed registry.
    #[error(
        "registry {registry}: nodeKeyed() could not be read; point it at a libID-contracts 0.21 IdentityRegistry"
    )]
    Unread {
        /// The registry asked.
        registry: Address,
        /// The call's failure.
        #[source]
        source: contract::Error,
    },
    /// The registry says it is not keyed by nodes.
    #[error("registry {registry} is not keyed by nodes")]
    NotNodeKeyed {
        /// The registry asked.
        registry: Address,
    },
}

/// Why an escrow is refused beside the registry an indexer follows.
#[derive(Debug, thiserror::Error)]
pub enum EscrowRefused {
    /// The escrow's `registry()` could not be read.
    #[error("escrow {escrow}: registry() could not be read")]
    Unread {
        /// The escrow asked.
        escrow: Address,
        /// The call's failure.
        #[source]
        source: contract::Error,
    },
    /// The escrow pays the holders of another registry.
    #[error(
        "escrow {escrow} resolves holders through registry {registry}, not {indexed}"
    )]
    OtherRegistry {
        /// The escrow asked.
        escrow: Address,
        /// The registry it resolves holders through.
        registry: Address,
        /// The registry the indexer follows.
        indexed: Address,
    },
}
