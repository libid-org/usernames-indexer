//! Referrals against a real chain: anvil runs the registry, the escrow and
//! the `ReferralSplitter` a deployment runs, binds pay their fee through the
//! splitter, and the indexer's own loop indexes them into Postgres, where the
//! API answers. Every number the API serves is reconciled against the
//! contracts' own balances and views.
//!
//! Skips without `DATABASE_URL` or `anvil`, unless `CI` is set: there a
//! missing one fails. Everything past those checks panics on failure.

use std::time::Duration;

use alloy::{
    primitives::{
        Address,
        U256,
    },
    providers::{
        ext::AnvilApi,
        Provider,
    },
};
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressReferrals,
        Delivery,
        EscrowAmount,
        HandleAmounts,
        Outcome,
    },
    chain::{
        self,
        Contracts,
    },
    db::{
        self,
        ChainStore,
    },
    indexer,
    nodes::KnownPlatform,
};
use usernames_fixtures::{
    anvil_available,
    database_url,
    libid::{
        Disclose,
        Handle,
        Libid,
        Node,
        Proof,
        Transacts,
        SHARE_BPS,
        TREASURY,
    },
    ChainWatch,
    Reads,
    Seed,
};

/// The indexed chain, apart from every other suite's.
const CHAIN: u64 = 43118;

/// A chain only the store knows, where a handle's holder is seeded: what
/// the splitter holds on the indexed chain waits for that holder there.
const SEEDED_CHAIN: i64 = 43119;

/// What each bind pays, above the splitter's minimum for a referral.
const FEE: u64 = 10_000;

/// The share of [`FEE`] at the splitter's rate.
fn share() -> U256 {
    U256::from(FEE) * U256::from(SHARE_BPS) / U256::from(10_000u64)
}

/// What the suite compares of a referrer's referrals.
trait Outcomes {
    /// Binder, outcome and delivery of each, newest first.
    fn outcomes(&self) -> Vec<(Address, Outcome, Option<Delivery>)>;
}

impl Outcomes for AddressReferrals {
    fn outcomes(&self) -> Vec<(Address, Outcome, Option<Delivery>)> {
        self.referrals
            .iter()
            .map(|r| (r.binder, r.outcome, r.delivery))
            .collect()
    }
}

#[tokio::test]
async fn indexes_the_splitters_referrals_and_reconciles_them() {
    let chain_id = CHAIN;
    let Some(url) = database_url() else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !anvil_available() {
        eprintln!("skipping: anvil not on PATH");
        return;
    }

    let pool = db::connect_and_migrate(&url).await.expect("database");
    let store = ChainStore::new(pool.clone(), chain_id as i64);
    store.forget().await;
    let seeded = ChainStore::new(pool.clone(), SEEDED_CHAIN);
    seeded.forget().await;

    let node = Node::spawn(chain_id);
    let provider = &node.provider;
    let &[owner, alice, bob, carol, dave, erin, frank, ..] = node.accounts() else {
        panic!("anvil has fewer than seven accounts");
    };
    let mut libid = Libid::deploy(provider, owner).await;
    let (x, google) = (KnownPlatform::X, KnownPlatform::Google);
    let fee = U256::from(FEE);
    let proof = |handle, id| Proof {
        handle,
        id,
        observed_at: 1000,
    };
    let (alice_r, erin_r, nobody_r) = (
        Handle(x, "alice_a"),
        Handle(x, "erin_a"),
        Handle(x, "nobody_a"),
    );

    // Alice and Erin hold handles; Erin's wallet then refuses every payment.
    libid
        .bind(alice, proof(alice_r, "111"), Disclose::Name, U256::ZERO)
        .await;
    libid
        .bind(erin, proof(erin_r, "555"), Disclose::Name, U256::ZERO)
        .await;
    libid.refuse_payments(erin).await;

    // Bob's share is forwarded to Alice; Carol's is held for @nobody_a, whom
    // nobody holds here; Alice referring herself and Dave referring Erin pay
    // the treasury the whole fee.
    let treasury_before = libid.balance(TREASURY).await;
    libid
        .bind_referred(bob, proof(Handle(x, "bob_a"), "222"), alice_r, fee)
        .await;
    libid
        .bind_referred(carol, proof(Handle(x, "carol_a"), "333"), nobody_r, fee)
        .await;
    libid
        .bind_referred(
            alice,
            proof(Handle(google, "alice.a@gmail.com"), "123456789012345678901"),
            alice_r,
            fee,
        )
        .await;
    libid
        .bind_referred(dave, proof(Handle(x, "dave_a"), "444"), erin_r, fee)
        .await;

    let latest = provider.get_block_number().await.expect("latest");
    provider.anvil_mine(Some(1), None).await.expect("mine");

    // Frank holds @nobody_a on another chain the store holds.
    seeded.bind("nobody_a", frank).await;

    let contracts = libid.contracts();
    contracts
        .check_referrals(provider)
        .await
        .expect("the splitter pays through the escrow of the registry");
    let mut lease = store.acquire_writer().await.expect("writer lease");
    let writer = store.prepare(&mut lease, contracts).await.expect("prepare");
    let cancel = CancellationToken::new();
    let config = indexer::IndexerConfig {
        contracts,
        confirmations: 1,
        poll_interval: Duration::from_millis(20),
        max_block_range: 3,
        start_block: None,
        stale_after: Duration::from_secs(120),
    };
    let task = tokio::spawn(
        indexer::Indexer::new(
            writer,
            node.rpc().try_into().expect("anvil serves http"),
            config,
        )
        .run(cancel.clone()),
    );
    ChainWatch::new(&pool, &store).await.cursor_at(latest).await;
    cancel.cancel();
    task.await
        .expect("the loop ends")
        .expect("the loop ends cleanly");
    lease.release().await.expect("the lease releases");

    let referrals = |holder: Address| {
        let pool = pool.clone();
        async move {
            pool.get_on::<AddressReferrals>(
                chain_id as i64,
                &format!("/v1/referrals/address/{holder}"),
            )
            .await
            .answer()
        }
    };
    let alices = referrals(alice).await;
    assert_eq!(
        alices.outcomes(),
        [
            (alice, Outcome::SelfReferral, None),
            (bob, Outcome::Paid, Some(Delivery::Forwarded)),
        ]
    );
    assert!(alices
        .referrals
        .iter()
        .all(|r| r.fee == fee && r.share == share()));
    assert_eq!(
        alices.referrals[0].binder_handle.as_deref(),
        Some("alice_a")
    );
    assert_eq!(
        referrals(erin).await.outcomes(),
        [(dave, Outcome::Refused, None)]
    );

    // Carol's share is Frank's, by the handle he holds elsewhere: in his
    // referrals as held, and awaiting him here, where the escrow holds it.
    let franks: AddressReferrals = pool
        .get(&format!("/v1/referrals/address/{frank}"))
        .await
        .answer();
    assert_eq!(
        franks.outcomes(),
        [(carol, Outcome::Paid, Some(Delivery::Held))]
    );
    let awaiting: AddressAmounts = pool
        .get(&format!("/v1/escrow/awaiting/{frank}"))
        .await
        .answer();
    let escrowed = libid.escrowed(nobody_r).await;
    assert_eq!(escrowed, share());
    let slots = |amounts: &[EscrowAmount]| {
        amounts
            .iter()
            .map(|a| (a.chain_id, a.handle.handle_node, a.amount))
            .collect::<Vec<_>>()
    };
    let nobody_node = libid.node(nobody_r).await;
    assert_eq!(
        slots(&awaiting.amounts),
        [(chain_id as i64, nobody_node, escrowed)]
    );
    // The holder's handle names the platform the node alone does not.
    assert_eq!(awaiting.amounts[0].handle.platform, Some(x));
    let held: HandleAmounts = pool
        .get_on(chain_id as i64, &format!("/v1/escrow/node/{nobody_node}"))
        .await
        .answer();
    assert_eq!(slots(&held.amounts), slots(&awaiting.amounts));

    // The money reconciles: the treasury took every fee less the two shares
    // the escrow took, and the splitter keeps nothing.
    let treasury = libid.balance(TREASURY).await - treasury_before;
    assert_eq!(
        treasury,
        fee * U256::from(4u64) - share() * U256::from(2u64)
    );
    assert_eq!(libid.balance(*libid.splitter.address()).await, U256::ZERO);
}

/// A splitter is followed only beside the escrow it pays through, of the
/// registry the indexer follows.
#[tokio::test]
async fn a_splitter_of_other_contracts_or_without_an_escrow_is_refused() {
    if !anvil_available() {
        eprintln!("skipping: anvil not on PATH");
        return;
    }
    let node = Node::spawn(CHAIN);
    let (provider, owner) = (&node.provider, node.accounts()[0]);
    let libid = Libid::deploy(provider, owner).await;
    let contracts = libid.contracts();
    let splitter = *libid.splitter.address();

    let without_escrow = Contracts {
        escrow: None,
        ..contracts
    };
    assert!(matches!(
        without_escrow.check_referrals(provider).await,
        Err(chain::ReferralsRefused::NoEscrow { splitter: s }) if s == splitter
    ));

    let other_escrow = provider.deploy_escrow(owner, contracts.registry).await;
    let refused = Contracts {
        escrow: Some(other_escrow),
        ..contracts
    }
    .check_referrals(provider)
    .await;
    assert!(
        matches!(
            refused,
            Err(chain::ReferralsRefused::OtherContracts { escrow, .. })
                if Some(escrow) == contracts.escrow
        ),
        "{refused:?}"
    );

    let other_registry = provider.deploy_registry(owner).await;
    let refused = Contracts {
        registry: other_registry,
        ..contracts
    }
    .check_referrals(provider)
    .await;
    assert!(
        matches!(
            refused,
            Err(chain::ReferralsRefused::OtherContracts { registry, .. })
                if registry == contracts.registry
        ),
        "{refused:?}"
    );
}
