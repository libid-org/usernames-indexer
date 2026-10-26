//! The libid contracts on anvil, wired as a deployment wires them: the
//! crate's `IdentityRegistry`, `CeremonyProofVerifier` and `HandleEscrow`
//! behind their proxies, with libid-contracts' `StubPlatformVerifier`
//! standing in for each platform's proof check.

use alloy::{
    contract::{
        CallBuilder,
        CallDecoder,
    },
    primitives::{
        Address,
        B256,
        U256,
    },
    providers::{
        DynProvider,
        Provider,
        ProviderBuilder,
    },
    rpc::types::TransactionReceipt,
    sol,
    sol_types::SolValue,
};
use libid_contracts::{
    bindings::{
        ceremony::CeremonyProofVerifier,
        escrow::HandleEscrow::{
            self,
            HandleEscrowInstance,
        },
        identity::IdentityRegistry::{
            self,
            IdentityRegistryInstance,
        },
    },
    deploy::deploy_behind_proxy,
    Artifacts,
};
use usernames_core::nodes::KnownPlatform;

use StubPlatformVerifier::StubPlatformVerifierInstance;

sol!(
    /// libid-contracts' stand-in for a Platform Verifier: it answers any
    /// payload with the nodes of the account and handle it was last `set` to.
    /// Compiled from libid-contracts v0.19.0; see contracts/README.md.
    #[sol(rpc)]
    StubPlatformVerifier,
    "../../contracts/StubPlatformVerifier.json"
);

sol! {
    /// The payload `StubPlatformVerifier.verify` decodes: what the
    /// authorization digest is built from.
    struct StubPayload {
        uint16 ceremonyVersion;
        bytes32 operationDomain;
        bytes32 authorizationNonce;
        bytes transactionData;
    }
}

/// The Proof Verifier's slot each platform's stub fills.
const VERIFIER_VERSION: u16 = 1;

/// The ceremony version every stubbed proof reports.
pub const CEREMONY_VERSION: u16 = 1;

/// Where a bind's service fee goes.
pub const FEE_RECEIVER: Address = Address::repeat_byte(0xFE);

/// The crate's compiled contracts, the ones a deployment ships.
const ARTIFACTS: Artifacts = Artifacts::embedded();

/// The platforms a stub verifies.
const PLATFORMS: [KnownPlatform; 2] = [KnownPlatform::X, KnownPlatform::Google];

/// A handle as a payer or a holder types it: its platform and its text, in
/// any case.
#[derive(Debug, Clone, Copy)]
pub struct Handle(pub KnownPlatform, pub &'static str);

/// What a stubbed proof reports: the account, the handle it goes by, and
/// when the platform was read.
#[derive(Debug, Clone, Copy)]
pub struct Proof {
    pub handle: Handle,
    pub id: &'static str,
    pub observed_at: u64,
}

/// What a bind tells the chain about the handle it proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disclose {
    /// `bindPrivately`: nodes only.
    Nothing,
    /// `bind` with the handle's text.
    Text,
    /// `bind` with the text, as the holder's name.
    Name,
}

/// Anvil with every dev account's key in the wallet, on `chain`.
pub fn anvil(chain: u64) -> DynProvider {
    ProviderBuilder::new()
        .connect_anvil_with_wallet_and_config(|anvil| anvil.chain_id(chain))
        .expect("anvil spawns")
        .erased()
}

/// The deployed contracts, and the stubs that verify for them.
pub struct Libid {
    pub registry: IdentityRegistryInstance<DynProvider>,
    pub escrow: HandleEscrowInstance<DynProvider>,
    pub proof_verifier: Address,
    /// The escrow's address for the chain's own coin.
    pub native: Address,
    /// The block the registry's proxy was deployed in.
    pub deployed_at: u64,
    stubs: Vec<(KnownPlatform, StubPlatformVerifierInstance<DynProvider>)>,
    /// Binds so far, which keeps each one's authorization nonce unique.
    binds: u64,
}

impl Libid {
    /// Deploy and wire the contracts, owned by `owner`: X and Google, each
    /// verified by a stub.
    pub async fn deploy(provider: &DynProvider, owner: Address) -> Self {
        let registry = IdentityRegistry::new(
            Self::registry(provider, owner).await,
            provider.clone(),
        );
        let deployed_at = provider.get_block_number().await.expect("block");
        let proof_verifier = CeremonyProofVerifier::new(
            deploy_behind_proxy(
                provider,
                &ARTIFACTS,
                "CeremonyProofVerifier",
                &CeremonyProofVerifier::initializeCall { owner_: owner },
                None,
            )
            .await
            .expect("deploy the Proof Verifier"),
            provider.clone(),
        );
        mined(registry.setProofVerifier(*proof_verifier.address())).await;

        let mut stubs = Vec::new();
        for platform in PLATFORMS {
            let stub =
                StubPlatformVerifier::deploy(provider.clone(), platform.id(), U256::ZERO)
                    .await
                    .expect("deploy a stub verifier");
            mined(proof_verifier.setVerifier(
                platform.id(),
                VERIFIER_VERSION,
                *stub.address(),
            ))
            .await;
            stubs.push((platform, stub));
        }

        let escrow = HandleEscrow::new(
            Self::escrow(provider, owner, *registry.address()).await,
            provider.clone(),
        );
        let native = escrow.NATIVE().call().await.expect("the native token");
        Self {
            registry,
            escrow,
            proof_verifier: *proof_verifier.address(),
            native,
            deployed_at,
            stubs,
            binds: 0,
        }
    }

    /// A registry behind its proxy.
    pub async fn registry(provider: &DynProvider, owner: Address) -> Address {
        deploy_behind_proxy(
            provider,
            &ARTIFACTS,
            "IdentityRegistry",
            &IdentityRegistry::initializeCall { owner_: owner },
            None,
        )
        .await
        .expect("deploy the registry")
    }

    /// An escrow behind its proxy, resolving holders through `registry`.
    pub async fn escrow(
        provider: &DynProvider,
        owner: Address,
        registry: Address,
    ) -> Address {
        deploy_behind_proxy(
            provider,
            &ARTIFACTS,
            "HandleEscrow",
            &HandleEscrow::initializeCall {
                owner_: owner,
                registry_: registry,
            },
            None,
        )
        .await
        .expect("deploy the escrow")
    }

    /// `holder` binds what `proof` reports, disclosing as `disclose` says
    /// and paying `fee` to [`FEE_RECEIVER`] when it is not zero.
    pub async fn bind(
        &mut self,
        holder: Address,
        proof: Proof,
        disclose: Disclose,
        fee: U256,
    ) {
        self.binds += 1;
        let Handle(platform, handle) = proof.handle;
        let (_, stub) = self
            .stubs
            .iter()
            .find(|(p, _)| *p == platform)
            .expect("a stub verifies the platform");
        mined(stub.set(proof.id.into(), handle.into())).await;
        mined(stub.setObservedAt(proof.observed_at)).await;

        let payload = StubPayload {
            ceremonyVersion: CEREMONY_VERSION,
            operationDomain: self
                .registry
                .OPERATION_DOMAIN()
                .call()
                .await
                .expect("the operation domain"),
            authorizationNonce: B256::from(U256::from(self.binds)),
            transactionData: holder.abi_encode().into(),
        }
        .abi_encode()
        .into();
        let fee_receiver = if fee.is_zero() {
            Address::ZERO
        } else {
            FEE_RECEIVER
        };
        let (platform, version) = (platform.id(), VERIFIER_VERSION);
        match disclose {
            Disclose::Nothing => {
                let call = self.registry.bindPrivately(
                    platform,
                    version,
                    payload,
                    fee,
                    fee_receiver,
                );
                mined(call.from(holder).value(fee)).await;
            }
            Disclose::Text | Disclose::Name => {
                let call = self.registry.bind(
                    platform,
                    version,
                    payload,
                    handle.into(),
                    disclose == Disclose::Name,
                    fee,
                    fee_receiver,
                );
                mined(call.from(holder).value(fee)).await;
            }
        }
    }

    /// `holder` names a handle it holds.
    pub async fn publish(&self, holder: Address, Handle(platform, handle): Handle) {
        mined(
            self.registry
                .publish(platform.id(), handle.into())
                .from(holder),
        )
        .await;
    }

    /// `holder` withdraws its name.
    pub async fn unpublish(&self, holder: Address) {
        mined(self.registry.unpublish().from(holder)).await;
    }

    /// `payer` sends `amount` of the chain's coin to a handle by its node,
    /// refundable to itself while it waits.
    pub async fn deposit(&self, payer: Address, handle: Handle, amount: u64) {
        let node = self.node(handle).await;
        let amount = U256::from(amount);
        mined(
            self.escrow
                .deposit(handle.0.id(), node, self.native, amount, payer)
                .from(payer)
                .value(amount),
        )
        .await;
    }

    /// The same, naming the handle's text on chain.
    pub async fn deposit_to_handle(
        &self,
        payer: Address,
        Handle(platform, handle): Handle,
        amount: u64,
    ) {
        let amount = U256::from(amount);
        mined(
            self.escrow
                .depositToHandle(platform.id(), handle.into(), self.native, amount, payer)
                .from(payer)
                .value(amount),
        )
        .await;
    }

    /// `holder` takes what waits for its handle.
    pub async fn claim(&self, holder: Address, handle: Handle) {
        let node = self.node(handle).await;
        mined(
            self.escrow
                .claim(node, vec![self.native], holder)
                .from(holder),
        )
        .await;
    }

    /// `payer` takes back what it put towards a handle.
    pub async fn refund(&self, payer: Address, handle: Handle) {
        let node = self.node(handle).await;
        mined(self.escrow.refund(node, self.native, payer).from(payer)).await;
    }

    /// The node the registry files a handle under.
    pub async fn node(&self, Handle(platform, handle): Handle) -> B256 {
        self.registry
            .handleNodeOf(platform.id(), handle.into())
            .call()
            .await
            .expect("the handle's node")
    }

    /// What the escrow holds for a handle.
    pub async fn escrowed(&self, handle: Handle) -> U256 {
        self.escrow
            .escrowed(self.node(handle).await, self.native)
            .call()
            .await
            .expect("escrowed")
    }

    /// What `refund` would pay `payer` from a handle now.
    pub async fn refundable(&self, handle: Handle, payer: Address) -> U256 {
        self.escrow
            .refundable(self.node(handle).await, self.native, payer)
            .call()
            .await
            .expect("refundable")
    }
}

/// Send a call and wait for its block; a revert fails the test.
async fn mined<P: Provider, D: CallDecoder>(
    call: CallBuilder<P, D>,
) -> TransactionReceipt {
    let receipt = call
        .send()
        .await
        .expect("the call is accepted")
        .get_receipt()
        .await
        .expect("the call is mined");
    assert!(receipt.status(), "reverted: {receipt:?}");
    receipt
}
