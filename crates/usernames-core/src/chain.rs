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

impl Contracts {
    /// Every contract whose logs the indexer reads.
    pub fn watched(&self) -> Vec<Address> {
        std::iter::once(self.registry)
            .chain(self.escrow)
            .chain(self.referral_splitter)
            .collect()
    }

    /// Decode a log by the contract that emitted it: each contract's topics
    /// mean something only from that contract. The filter admits no other
    /// emitter, so one would be an RPC answering a different question.
    pub fn decode(
        &self,
        log: &Log,
    ) -> Result<Option<(NamesEvent, LogPosition)>, DecodeError> {
        let emitter = log.address();
        if emitter == self.registry {
            NamesEvent::from_registry(log)
        } else if Some(emitter) == self.escrow {
            NamesEvent::from_escrow(log)
        } else if Some(emitter) == self.referral_splitter {
            NamesEvent::from_splitter(log)
        } else {
            tracing::warn!(
                %emitter,
                "a log from a contract the filter does not name; skipped"
            );
            Ok(None)
        }
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

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{
            self,
            U256,
        },
        sol_types::SolEvent,
    };

    use super::*;

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
}
