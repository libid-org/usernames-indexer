//! End to end against a real chain: anvil runs the libid contracts a
//! deployment runs — IdentityRegistry, CeremonyProofVerifier and HandleEscrow
//! behind their proxies — with a stub in place of each platform's proof
//! check. The indexer's own loop, deployment-block detection included,
//! indexes the registry and the escrow into Postgres, where the API answers.
//!
//! Skips in two cases: `DATABASE_URL` unset, or no `anvil` binary on PATH.
//! Everything past those checks panics on failure.

use std::{
    process::Command,
    time::Duration,
};
use tokio::time::Instant;

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
        ext::AnvilApi,
        DynProvider,
        Provider,
        ProviderBuilder,
    },
    rpc::types::TransactionReceipt,
    sol,
    sol_types::SolValue,
};
use axum::http::StatusCode;
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
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressHistory,
        AddressResolution,
        HandleAmounts,
        HandleHistory,
        HandleResolution,
        HistoryEvent,
        IdResolution,
        Role,
        SearchResults,
        Status,
        Unclaimed,
    },
    chain,
    db::{
        self,
        ChainStore,
    },
    indexer,
    nodes::{
        self,
        KnownPlatform,
        NormalizedHandle,
    },
};

mod common;
use common::{
    events,
    Reply,
};

/// A request scoped to this suite's chain: the store is shared with the
/// read-model suite's chain.
async fn get<T: DeserializeOwned>(pool: &PgPool, path: &str) -> Reply<T> {
    let separator = if path.contains('?') { '&' } else { '?' };
    common::get(pool, &format!("{path}{separator}chain={CHAIN}")).await
}

sol!(
    /// libid-contracts' stand-in for a Platform Verifier: it answers any
    /// payload with the account and handle it was last `set` to. Compiled
    /// from libid-contracts v0.15.0; see contracts/README.md.
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

/// Not 31337: the read-model tests use anvil's default id against the same
/// database, and chain_id keying is the isolation this exercises.
const CHAIN: u64 = 43117;

/// A Google account id as the chain binds it: `0x` and the hex SHA-256 of
/// `"libid.google-user-id" || sub`. This is libid-contracts'
/// `GooglePlatformVerifier` test vector for the sub `123456789012345678901`.
const GOOGLE_USER_ID: &str =
    "0x20078023c9d4bf6bffc2580ec36446075d10c8453cecbe4f1cb3d326b2b35560";

/// The Proof Verifier's slot each platform's stub fills.
const VERIFIER_VERSION: u16 = 1;

/// The ceremony version every stubbed proof reports.
const CEREMONY_VERSION: u16 = 1;

/// Where a bind's service fee goes.
const FEE_RECEIVER: Address = Address::repeat_byte(0xFE);

/// The crate's compiled contracts, the ones a deployment ships.
const ARTIFACTS: Artifacts = Artifacts::embedded();

/// A handle as a payer names it: its platform and its text, in any case.
#[derive(Clone, Copy)]
struct Handle(KnownPlatform, &'static str);

/// What a stubbed proof reports: the account, the handle it goes by, and
/// when the platform was read.
struct Proof {
    handle: Handle,
    id: &'static str,
    observed_at: u64,
}

/// The libid contracts on anvil, wired as a deployment wires them, with a
/// [`StubPlatformVerifier`] as each platform's verifier.
struct Libid {
    registry: IdentityRegistryInstance<DynProvider>,
    proof_verifier: Address,
    escrow: HandleEscrowInstance<DynProvider>,
    stubs: Vec<(
        KnownPlatform,
        StubPlatformVerifier::StubPlatformVerifierInstance<DynProvider>,
    )>,
    /// The escrow's address for the chain's own coin.
    native: Address,
    /// The block the registry's proxy was deployed in.
    deployed_at: u64,
    /// Binds so far, which keeps each one's authorization nonce unique.
    binds: u64,
}

impl Libid {
    /// Deploy and wire the contracts, owned by `owner`: X and Google
    /// configured with their release rules, each verified by a stub.
    async fn deploy(provider: &DynProvider, owner: Address) -> Self {
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
        for platform in [KnownPlatform::X, KnownPlatform::Google] {
            let rules = platform.rules().expect("a known platform has rules");
            let rules = IdentityRegistry::Rules {
                maxLength: u16::try_from(rules.max_length).expect("a u16 length"),
                stripLeadingAt: rules.strip_leading_at,
                isEmail: rules.is_email,
                allowUnderscore: rules.allow_underscore,
                allowHyphen: rules.allow_hyphen,
            };
            mined(registry.setPlatform(platform.id(), rules)).await;
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
            proof_verifier: *proof_verifier.address(),
            escrow,
            stubs,
            native,
            deployed_at,
            binds: 0,
        }
    }

    /// A registry behind its proxy.
    async fn registry(provider: &DynProvider, owner: Address) -> Address {
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
    async fn escrow(
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

    /// `holder` binds what `proof` reports, paying `fee` to [`FEE_RECEIVER`]
    /// when it is not zero.
    async fn bind(&mut self, holder: Address, proof: Proof, publish: bool, fee: U256) {
        self.binds += 1;
        let Handle(platform, handle) = proof.handle;
        let (_, stub) = self
            .stubs
            .iter()
            .find(|(p, _)| *p == platform)
            .expect("a stub verifies the platform");
        mined(stub.set(proof.id.into(), handle.into())).await;
        mined(stub.setObservedAt(proof.observed_at)).await;

        let fee_receiver = if fee.is_zero() {
            Address::ZERO
        } else {
            FEE_RECEIVER
        };
        let payload = StubPayload {
            ceremonyVersion: CEREMONY_VERSION,
            operationDomain: self
                .registry
                .OPERATION_DOMAIN()
                .call()
                .await
                .expect("the operation domain"),
            authorizationNonce: B256::from(U256::from(self.binds)),
            transactionData: (holder, fee, fee_receiver).abi_encode_params().into(),
        };
        mined(
            self.registry
                .bind(
                    platform.id(),
                    VERIFIER_VERSION,
                    payload.abi_encode().into(),
                    publish,
                )
                .from(holder)
                .value(fee),
        )
        .await;
    }

    /// `holder` withdraws its published handle on `platform`.
    async fn unpublish(&self, holder: Address, platform: KnownPlatform) {
        mined(self.registry.unpublish(platform.id()).from(holder)).await;
    }

    /// `payer` sends `amount` of the chain's coin to a handle, refundable to
    /// itself while it waits.
    async fn deposit(
        &self,
        payer: Address,
        Handle(platform, handle): Handle,
        amount: u64,
    ) {
        let hash = self
            .registry
            .handleHashOf(platform.id(), handle.into())
            .call()
            .await
            .expect("the handle's hash");
        let amount = U256::from(amount);
        mined(
            self.escrow
                .deposit(platform.id(), hash, self.native, amount, payer)
                .from(payer)
                .value(amount),
        )
        .await;
    }

    /// `holder` takes what waits for its handle.
    async fn claim(&self, holder: Address, handle: Handle) {
        let node = self.node(handle).await;
        mined(
            self.escrow
                .claim(node, vec![self.native], holder)
                .from(holder),
        )
        .await;
    }

    /// `payer` takes back what it put towards a handle.
    async fn refund(&self, payer: Address, handle: Handle) {
        let node = self.node(handle).await;
        mined(self.escrow.refund(node, self.native, payer).from(payer)).await;
    }

    /// The node the registry files a handle under.
    async fn node(&self, Handle(platform, handle): Handle) -> B256 {
        self.registry
            .handleNodeOf(platform.id(), handle.into())
            .call()
            .await
            .expect("the handle's node")
    }

    /// What the escrow holds for a handle.
    async fn escrowed(&self, handle: Handle) -> U256 {
        self.escrow
            .escrowed(self.node(handle).await, self.native)
            .call()
            .await
            .expect("escrowed")
    }

    /// What `refund` would pay `payer` from a handle now.
    async fn refundable(&self, handle: Handle, payer: Address) -> U256 {
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

/// Anvil with every dev account's key in the wallet, or `None` when there is
/// no anvil binary.
fn anvil() -> Option<DynProvider> {
    if Command::new("anvil").arg("--version").output().is_err() {
        eprintln!("skipping: anvil not on PATH");
        return None;
    }
    let provider = ProviderBuilder::new()
        .connect_anvil_with_wallet_and_config(|anvil| anvil.chain_id(CHAIN))
        .expect("anvil spawns");
    Some(provider.erased())
}

#[tokio::test]
async fn indexes_a_real_chain_end_to_end() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let Some(provider) = anvil() else { return };

    let pool = db::connect_and_migrate(&url).await.expect("database");
    let store = ChainStore::new(pool.clone(), CHAIN as i64);
    // A previous run of this test left rows under this chain id; the loop
    // must start from a clean slate to make block-number assertions exact.
    common::clear_chain(&pool, CHAIN as i64).await;

    let accounts = provider.get_accounts().await.expect("accounts");
    let &[owner, alice, bob, payer, refunder, ..] = &accounts[..] else {
        panic!("anvil has fewer than five accounts: {accounts:?}");
    };

    // Blocks before the deploy give the binary search something to find.
    provider.anvil_mine(Some(5), None).await.expect("mine");
    let mut libid = Libid::deploy(&provider, owner).await;
    let (x, google) = (KnownPlatform::X, KnownPlatform::Google);

    // Alice binds @alice_1 published and pays the app's fee, renames to
    // @alice_2, adds a Google identity, and withdraws the published handle.
    let alice_1 = Proof {
        handle: Handle(x, "alice_1"),
        id: "111",
        observed_at: 1000,
    };
    libid.bind(alice, alice_1, true, U256::from(5u64)).await;
    let alice_2 = Proof {
        handle: Handle(x, "alice_2"),
        id: "111",
        observed_at: 2000,
    };
    libid.bind(alice, alice_2, true, U256::ZERO).await;
    let alice_google = Proof {
        handle: Handle(google, "A.B+tag@Example.COM"),
        id: GOOGLE_USER_ID,
        observed_at: 3000,
    };
    libid.bind(alice, alice_google, false, U256::ZERO).await;
    libid.unpublish(alice, x).await;

    // A payer escrows for @bob_x, whom nobody holds yet, pays Alice's held
    // @alice_2 straight through, and escrows for @carol_x. A second payer
    // escrows for @carol_x, takes it back, and escrows again. Bob then binds
    // @bob_x and claims what waited; nobody binds @carol_x.
    let (bob_x, carol_x) = (Handle(x, "bob_x"), Handle(x, "carol_x"));
    libid.deposit(payer, bob_x, 100).await;
    libid.deposit(payer, Handle(x, "alice_2"), 7).await;
    libid.deposit(payer, carol_x, 30).await;
    libid.deposit(refunder, carol_x, 20).await;
    libid.refund(refunder, carol_x).await;
    libid.deposit(refunder, carol_x, 15).await;
    let bob_proof = Proof {
        handle: bob_x,
        id: "222",
        observed_at: 4000,
    };
    libid.bind(bob, bob_proof, false, U256::ZERO).await;
    libid.claim(bob, bob_x).await;

    let latest = provider.get_block_number().await.expect("latest");
    let (registry, escrow) = (*libid.registry.address(), *libid.escrow.address());

    // What the binary does before its loop: check the escrow resolves
    // through the indexed registry, take the chain's writer lease and
    // prepare the chain, which records the contracts the rows come from.
    chain::check_escrow(&provider, escrow, registry)
        .await
        .expect("the escrow resolves holders through the registry");
    let writer = store.acquire_writer().await.expect("writer lease");
    store
        .prepare(&writer, registry, Some(escrow))
        .await
        .expect("prepare");

    // Run the real loop: no start override (detection must find the deploy
    // block), a tiny window so catch-up spans several chunks, no
    // confirmation lag on a chain that cannot reorg.
    let cancel = CancellationToken::new();
    let config = indexer::IndexerConfig {
        contract: registry,
        escrow: Some(escrow),
        confirmations: 0,
        poll_interval: Duration::from_secs(1),
        max_block_range: 2,
        start_block: None,
        // A report good for two minutes; the loop renews it every cycle.
        stale_after: Duration::from_secs(120),
    };
    let task = tokio::spawn(
        indexer::Indexer::new(store.clone(), provider.clone(), config)
            .run(cancel.clone()),
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let cursor = store.cursor().await.expect("cursor");
        if cursor.is_some_and(|c| c >= latest) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "indexer did not catch up to block {latest}; cursor {cursor:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    cancel.cancel();
    let _ = task.await;
    writer.release().await.expect("the lease releases");

    // Detection ran once and its answer was cached — and it is the right one.
    let cached = store.deploy_block(registry).await.expect("metadata");
    assert_eq!(cached, Some(libid.deployed_at), "cached deployment block");

    // The indexer files a handle under the node the registry does.
    assert_eq!(
        nodes::handle_node(x.id(), &NormalizedHandle::from_chain("bob_x")),
        libid.node(bob_x).await
    );

    // alice_1 retired, alice_2 resolves, and the id followed the rename.
    let (status, _) = get::<HandleResolution>(&pool, "/v1/resolve/handle/x/alice_1")
        .await
        .refusal();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/x/alice_2").await.answer();
    let binding = &resolved.bindings[0];
    assert_eq!(binding.chain_id, CHAIN as i64);
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.ceremony_version, i64::from(CEREMONY_VERSION));
    let resolved: IdResolution = get(&pool, "/v1/resolve/id/x/111").await.answer();
    assert_eq!(resolved.bindings[0].handle.as_deref(), Some("alice_2"));

    // The registry folded the Google address it was proved as; the API finds
    // it through the URL-encoded raw form.
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/google/A.B%2Btag%40Example.COM")
            .await
            .answer();
    assert_eq!(resolved.handle, "a.b+tag@example.com");
    assert_eq!(
        resolved.bindings[0].user_id.as_deref(),
        Some(GOOGLE_USER_ID)
    );
    let resolved: IdResolution =
        get(&pool, &format!("/v1/resolve/id/google/{GOOGLE_USER_ID}"))
            .await
            .answer();
    assert_eq!(resolved.bindings[0].owner, alice);
    assert_eq!(
        resolved.bindings[0].handle.as_deref(),
        Some("a.b+tag@example.com")
    );

    // Reverse: both identities, neither displayed (x was unpublished, google
    // never was).
    let resolved: AddressResolution = get(&pool, &format!("/v1/resolve/address/{alice}"))
        .await
        .answer();
    assert_eq!(resolved.identities.len(), 2, "{resolved:?}");
    assert!(
        resolved.identities.iter().all(|i| !i.published),
        "{resolved:?}"
    );
    assert!(
        resolved.identities.iter().all(|i| i.resolves),
        "{resolved:?}"
    );

    // Search sees the current handle, not the retired one.
    let results: SearchResults =
        get(&pool, "/v1/search?q=alice&platform=x").await.answer();
    let handles: Vec<&str> = results.hits.iter().map(|h| h.handle.as_str()).collect();
    assert_eq!(handles, ["alice_2"], "{results:?}");

    // Ops metadata filled from the admin events, on this chain's entry of
    // the status.
    let status: Status = common::get(&pool, "/v1/status").await.answer();
    let chain = status
        .chains
        .iter()
        .find(|c| c.chain_id == CHAIN as i64)
        .unwrap_or_else(|| panic!("chain {CHAIN} is listed: {status:?}"));
    assert_eq!(chain.contract, Some(registry));
    // The loop reports beside every target it sets, with an expiry in the
    // future, and the API surfaces both: this is what tells a caught-up
    // index from one whose indexer stopped.
    assert!(
        chain.indexer_reported_at.is_some(),
        "no report after a real loop ran: {chain:?}"
    );
    assert!(
        chain.report_valid_for.is_some_and(|s| s > 0),
        "a fresh report must still be valid: {chain:?}"
    );
    assert!(
        chain.last_indexed_block.is_some_and(|b| b >= latest),
        "{chain:?}"
    );
    assert_eq!(chain.proof_verifier, Some(libid.proof_verifier));
    assert_eq!(chain.escrow, Some(escrow));
    let platform_key: Option<String> = sqlx::query_scalar(
        "SELECT platform_key FROM names.platforms WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(CHAIN as i64)
    .bind(x.id().as_slice())
    .fetch_one(&pool)
    .await
    .expect("platform row");
    assert_eq!(platform_key.as_deref(), Some("x"));

    // The journal holds every event the registry and the escrow emitted: the
    // Proof Verifier and two platforms configured; three binds of Alice's
    // and one of Bob's, each with its ceremony, the first with its fee and
    // the rename with its retirement; the withdrawal; six escrow moves and
    // the claim.
    let journal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.events WHERE chain_id = $1")
            .bind(CHAIN as i64)
            .fetch_one(&pool)
            .await
            .expect("journal count");
    assert_eq!(journal, 21);
    let fee_kinds: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM names.events WHERE chain_id = $1 AND kind = 'bind_fee_paid'",
    )
    .bind(CHAIN as i64)
    .fetch_one(&pool)
    .await
    .expect("fee count");
    assert_eq!(fee_kinds, 1);

    // Both emitters' logs, in one history: the handle's from the deposit made
    // while nobody held it, through the bind and its ceremony, to the claim,
    // each dated by its block.
    let history: HandleHistory = get(&pool, "/v1/history/handle/x/bob_x").await.answer();
    assert!(
        matches!(
            events(&history.entries)[..],
            [
                HistoryEvent::Claimed { .. },
                HistoryEvent::CeremonyBound { .. },
                HistoryEvent::IdentityBound { .. },
                HistoryEvent::Deposited { .. },
            ]
        ),
        "{history:?}"
    );
    assert!(history.entries.iter().all(|e| e.block_time > 0));
    // The payer's history ends with Bob's claim taking its deposit.
    let history: AddressHistory = get(&pool, &format!("/v1/history/address/{payer}"))
        .await
        .answer();
    assert!(
        matches!(
            events(&history.entries)[..],
            [
                HistoryEvent::Claimed { .. },
                HistoryEvent::Deposited { .. },
                HistoryEvent::Forwarded { .. },
                HistoryEvent::Deposited { .. },
            ]
        ),
        "{history:?}"
    );
    assert_eq!(history.entries[0].roles, [Role::RefundTo]);
    assert_eq!(history.entries[3].roles, [Role::Depositor, Role::RefundTo]);
    let history: AddressHistory = get(&pool, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Holder]);
    assert!(matches!(
        history.entries[0].event,
        HistoryEvent::Forwarded { received, .. } if received == U256::from(7u64)
    ));
    let history: AddressHistory = get(&pool, &format!("/v1/history/address/{refunder}"))
        .await
        .answer();
    assert!(
        matches!(
            events(&history.entries)[..],
            [
                HistoryEvent::Deposited { .. },
                HistoryEvent::Refunded { .. },
                HistoryEvent::Deposited { .. },
            ]
        ),
        "{history:?}"
    );

    // The books match the escrow's own: Bob's slot is empty, and what waits
    // for @carol_x is what each payer could take back.
    let bobs: AddressAmounts = get(&pool, &format!("/v1/escrow/claimable/{bob}"))
        .await
        .answer();
    assert!(bobs.amounts.is_empty(), "{bobs:?}");
    assert_eq!(libid.escrowed(bob_x).await, U256::ZERO);
    let carols: HandleAmounts = get(&pool, "/v1/escrow/handle/x/carol_x").await.answer();
    assert_eq!(carols.amounts.len(), 1, "{carols:?}");
    assert_eq!(carols.amounts[0].amount, libid.escrowed(carol_x).await);
    assert_eq!(carols.amounts[0].amount, U256::from(45u64));
    let unclaimed: Unclaimed = get(&pool, "/v1/escrow/unclaimed").await.answer();
    assert_eq!(unclaimed.amounts, carols.amounts);
    for (depositor, put_in) in [(payer, 30u64), (refunder, 15)] {
        let refundable: AddressAmounts =
            get(&pool, &format!("/v1/escrow/refundable/{depositor}"))
                .await
                .answer();
        assert_eq!(refundable.amounts.len(), 1, "{refundable:?}");
        let amount = refundable.amounts[0].amount;
        assert_eq!(amount, libid.refundable(carol_x, depositor).await);
        assert_eq!(amount, U256::from(put_in));
    }
}

#[tokio::test]
async fn an_escrow_of_another_registry_is_refused() {
    let Some(provider) = anvil() else { return };
    let owner = provider.get_accounts().await.expect("accounts")[0];
    let indexed = Libid::registry(&provider, owner).await;
    let other = Libid::registry(&provider, owner).await;
    let escrow = Libid::escrow(&provider, owner, other).await;

    let refused = chain::check_escrow(&provider, escrow, indexed).await;
    assert!(
        matches!(
            refused,
            Err(chain::EscrowRefused::OtherRegistry { registry, indexed: asked, .. })
                if registry == other && asked == indexed
        ),
        "{refused:?}"
    );
}
