//! The libid contracts on anvil, wired as a deployment wires them: the
//! crate's `IdentityRegistry`, `CeremonyProofVerifier` and `HandleEscrow`
//! behind their proxies, with the crate's `StubPlatformVerifier` (feature
//! `testing`) standing in for each platform's proof check.

use std::future::Future;

use alloy::{
    node_bindings::{
        Anvil,
        AnvilInstance,
    },
    primitives::{
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::{
        ext::AnvilApi,
        DynProvider,
        Provider,
        ProviderBuilder,
    },
    rpc::types::{
        TransactionReceipt,
        TransactionRequest,
    },
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
    testing::{
        deploy_stub_platform_verifier,
        stub_payload,
        StubPayload,
        StubPlatformVerifier::{
            self,
            StubPlatformVerifierInstance,
        },
    },
    Artifacts,
};
use url::Url;
use usernames_core::{
    chain::Contracts,
    nodes::KnownPlatform,
};

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
    /// The handle the account goes by.
    pub handle: Handle,
    /// The account's user id on the platform.
    pub id: &'static str,
    /// When the platform was read, in seconds since the epoch.
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

/// Anvil on one chain id, and a provider whose wallet holds every dev
/// account's key.
pub struct Node {
    /// A provider that signs as any dev account.
    pub provider: DynProvider,
    instance: AnvilInstance,
}

impl Node {
    /// Anvil on `chain`.
    pub fn spawn(chain: u64) -> Self {
        let instance = Anvil::new().chain_id(chain).spawn();
        let provider = ProviderBuilder::new()
            .wallet(instance.wallet().expect("anvil's dev accounts"))
            .connect_http(instance.endpoint_url())
            .erased();
        Self { provider, instance }
    }

    /// The JSON-RPC endpoint an indexer dials.
    pub fn rpc(&self) -> Url {
        self.instance.endpoint_url()
    }

    /// The dev accounts; the first deploys.
    pub fn accounts(&self) -> &[Address] {
        self.instance.addresses()
    }
}

/// The deployed contracts, and the stubs that verify for them.
pub struct Libid {
    /// The `IdentityRegistry`, behind its proxy.
    pub registry: IdentityRegistryInstance<DynProvider>,
    /// The `HandleEscrow`, behind its proxy.
    pub escrow: HandleEscrowInstance<DynProvider>,
    /// The `CeremonyProofVerifier` the registry checks proofs with.
    pub proof_verifier: Address,
    /// The escrow's address for the chain's own coin.
    pub native: Address,
    /// The block the registry's proxy was deployed in.
    pub deployed_at: u64,
    stubs: Vec<(KnownPlatform, StubPlatformVerifierInstance<DynProvider>)>,
    /// Proofs so far, which keeps each one's authorization nonce unique.
    proofs: u64,
}

impl Libid {
    /// The deployed registry and escrow, as an indexer follows them.
    pub fn contracts(&self) -> Contracts {
        Contracts {
            registry: *self.registry.address(),
            escrow: Some(*self.escrow.address()),
        }
    }

    /// Deploy and wire the contracts, owned by `owner`: X and Google, each
    /// verified by a stub.
    pub async fn deploy(provider: &DynProvider, owner: Address) -> Self {
        let registry = IdentityRegistry::new(
            provider.deploy_registry(owner).await,
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
        let wire = registry
            .setProofVerifier(*proof_verifier.address())
            .into_transaction_request();
        provider.mined(wire).await;

        let mut stubs = Vec::new();
        for platform in PLATFORMS {
            let stub = deploy_stub_platform_verifier(
                provider,
                &ARTIFACTS,
                platform.id(),
                U256::ZERO,
                None,
            )
            .await
            .expect("deploy a stub verifier");
            let wire = proof_verifier
                .setVerifier(platform.id(), VERIFIER_VERSION, stub)
                .into_transaction_request();
            provider.mined(wire).await;
            stubs.push((platform, StubPlatformVerifier::new(stub, provider.clone())));
        }

        let escrow = HandleEscrow::new(
            provider.deploy_escrow(owner, *registry.address()).await,
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
            proofs: 0,
        }
    }

    /// Point the platform's stub at what `proof` reports; the payload of
    /// `holder`'s bind of it.
    pub async fn prove(&mut self, holder: Address, proof: Proof) -> Bytes {
        self.proofs += 1;
        let Handle(platform, handle) = proof.handle;
        let (_, stub) = self
            .stubs
            .iter()
            .find(|(p, _)| *p == platform)
            .expect("a stub verifies the platform");
        let provider = self.registry.provider();
        provider
            .mined(
                stub.set(proof.id.into(), handle.into())
                    .into_transaction_request(),
            )
            .await;
        provider
            .mined(
                stub.setObservedAt(proof.observed_at)
                    .into_transaction_request(),
            )
            .await;

        stub_payload(&StubPayload {
            ceremonyVersion: CEREMONY_VERSION,
            operationDomain: self
                .registry
                .OPERATION_DOMAIN()
                .call()
                .await
                .expect("the operation domain"),
            authorizationNonce: B256::from(U256::from(self.proofs)),
            transactionData: holder.abi_encode().into(),
        })
    }

    /// `holder`'s bind of a proved `payload`, unsent: disclosing as
    /// `disclose` says, and paying `fee` to [`FEE_RECEIVER`] when it is not
    /// zero.
    pub fn binding(
        &self,
        holder: Address,
        Handle(platform, handle): Handle,
        payload: Bytes,
        disclose: Disclose,
        fee: U256,
    ) -> TransactionRequest {
        let fee_receiver = if fee.is_zero() {
            Address::ZERO
        } else {
            FEE_RECEIVER
        };
        let (platform, version) = (platform.id(), VERIFIER_VERSION);
        let request = match disclose {
            Disclose::Nothing => self
                .registry
                .bindPrivately(platform, version, payload, fee, fee_receiver)
                .into_transaction_request(),
            Disclose::Text | Disclose::Name => self
                .registry
                .bind(
                    platform,
                    version,
                    payload,
                    handle.into(),
                    disclose == Disclose::Name,
                    fee,
                    fee_receiver,
                )
                .into_transaction_request(),
        };
        request.from(holder).value(fee)
    }

    /// `holder` binds what `proof` reports; the block the bind landed in.
    pub async fn bind(
        &mut self,
        holder: Address,
        proof: Proof,
        disclose: Disclose,
        fee: U256,
    ) -> u64 {
        let payload = self.prove(holder, proof).await;
        let request = self.binding(holder, proof.handle, payload, disclose, fee);
        self.send(request).await
    }

    /// `holder` withdraws its name.
    pub async fn unpublish(&self, holder: Address) {
        let request = self.registry.unpublish().into_transaction_request();
        self.send(request.from(holder)).await;
    }

    /// `payer` sends `amount` of the chain's coin to a handle by its node,
    /// which only `payer` may refund, from its refund date, while it waits;
    /// the block it landed in.
    pub async fn deposit(&self, payer: Address, handle: Handle, amount: u64) -> u64 {
        let node = self.node(handle).await;
        let amount = U256::from(amount);
        let request = self
            .escrow
            .deposit(handle.0.id(), node, self.native, amount)
            .into_transaction_request();
        self.send(request.from(payer).value(amount)).await
    }

    /// The same, naming the handle's text on chain.
    pub async fn deposit_to_handle(
        &self,
        payer: Address,
        Handle(platform, handle): Handle,
        amount: u64,
    ) -> u64 {
        let amount = U256::from(amount);
        let request = self
            .escrow
            .depositToHandle(platform.id(), handle.into(), self.native, amount)
            .into_transaction_request();
        self.send(request.from(payer).value(amount)).await
    }

    /// `holder` takes what waits for its handle.
    pub async fn claim(&self, holder: Address, handle: Handle) {
        let node = self.node(handle).await;
        let request = self
            .escrow
            .claim(node, vec![self.native], holder)
            .into_transaction_request();
        self.send(request.from(holder)).await;
    }

    /// `payer` takes back what it put towards a handle, once its refund date
    /// has come.
    pub async fn refund(&self, payer: Address, handle: Handle) {
        let node = self.node(handle).await;
        let request = self
            .escrow
            .refund(node, self.native, payer)
            .into_transaction_request();
        self.send(request.from(payer)).await;
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

    /// What `payer` has booked against a handle, and when `refund` opens for
    /// it.
    pub async fn contribution(&self, handle: Handle, payer: Address) -> (U256, u64) {
        let booked = self
            .escrow
            .contribution(self.node(handle).await, self.native, payer)
            .call()
            .await
            .expect("contribution");
        (booked.amount, booked.refundableAt)
    }

    /// Move anvil's clock past every refund date set so far.
    pub async fn pass_refund_grace(&self) {
        let grace = self.escrow.REFUND_GRACE().call().await.expect("the grace");
        self.registry
            .provider()
            .anvil_increase_time(grace.to::<u64>())
            .await
            .expect("the clock moves");
    }

    /// Send `request`; the block it landed in.
    async fn send(&self, request: TransactionRequest) -> u64 {
        self.registry
            .provider()
            .mined(request)
            .await
            .block_number
            .expect("a mined transaction")
    }
}

/// What a suite sends to its chain.
pub trait Transacts {
    /// Send a transaction and wait for its block; a revert fails the test.
    fn mined(
        &self,
        request: TransactionRequest,
    ) -> impl Future<Output = TransactionReceipt> + Send;

    /// A registry behind its proxy, owned by `owner`.
    fn deploy_registry(&self, owner: Address) -> impl Future<Output = Address> + Send;

    /// An escrow behind its proxy, owned by `owner` and resolving holders
    /// through `registry`.
    fn deploy_escrow(
        &self,
        owner: Address,
        registry: Address,
    ) -> impl Future<Output = Address> + Send;
}

impl Transacts for DynProvider {
    async fn deploy_registry(&self, owner: Address) -> Address {
        deploy_behind_proxy(
            self,
            &ARTIFACTS,
            "IdentityRegistry",
            &IdentityRegistry::initializeCall { owner_: owner },
            None,
        )
        .await
        .expect("deploy the registry")
    }

    async fn deploy_escrow(&self, owner: Address, registry: Address) -> Address {
        deploy_behind_proxy(
            self,
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

    async fn mined(&self, request: TransactionRequest) -> TransactionReceipt {
        let receipt = self
            .send_transaction(request)
            .await
            .expect("the transaction is accepted")
            .get_receipt()
            .await
            .expect("the transaction is mined");
        assert!(receipt.status(), "reverted: {receipt:?}");
        receipt
    }
}
