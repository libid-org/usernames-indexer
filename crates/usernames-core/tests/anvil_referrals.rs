//! Referrals against a real chain: anvil runs the registry, the escrow and
//! the `ReferralSplitter` a deployment runs, binds pay their fee through the
//! splitter, and the indexer's own loop indexes them into Postgres, where the
//! API answers. Every number the API serves is reconciled against the
//! contracts' own balances and views.
//!
//! Runs against the Postgres at `DATABASE_URL` and `anvil` on PATH, and
//! fails without either.

use std::time::Duration;

use alloy::{
    primitives::{
        Address,
        B256,
        U256,
    },
    providers::{
        ext::AnvilApi,
        Provider,
    },
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressHistory,
        AddressReferrals,
        Delivery,
        EscrowAmount,
        HandleAmounts,
        HistoryEvent,
        Outcome,
        Status,
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
    database_url,
    libid::{
        Disclose,
        Handle,
        Libid,
        Node,
        Proof,
        Transacts,
        BIND_FEE,
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

/// The two chains of the journey across chains, apart from every other
/// suite's.
const CHAIN_A: u64 = 43145;
const CHAIN_B: u64 = 43146;

/// The share of [`BIND_FEE`] at the splitter's rate.
fn share() -> U256 {
    U256::from(BIND_FEE) * U256::from(SHARE_BPS) / U256::from(10_000u64)
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

/// What the journey compares of a referrer's referrals across chains.
trait OnChains {
    /// Chain, binder, outcome and delivery of each, by chain: binds on two
    /// chains in the same second list by chain, not by when they happened.
    fn on_chains(&self) -> Vec<(i64, Address, Outcome, Option<Delivery>)>;

    /// Chain and transaction of each, by chain, as [`OnChains::on_chains`].
    fn transactions(&self) -> Vec<(i64, B256)>;
}

impl OnChains for AddressReferrals {
    fn on_chains(&self) -> Vec<(i64, Address, Outcome, Option<Delivery>)> {
        let mut rows: Vec<_> = self
            .referrals
            .iter()
            .map(|r| (r.chain_id, r.binder, r.outcome, r.delivery))
            .collect();
        rows.sort_by_key(|&(chain, ..)| chain);
        rows
    }

    fn transactions(&self) -> Vec<(i64, B256)> {
        let mut rows: Vec<_> = self
            .referrals
            .iter()
            .map(|r| (r.chain_id, r.tx_hash))
            .collect();
        rows.sort_by_key(|&(chain, _)| chain);
        rows
    }
}

/// What the suite compares of the escrow's amounts.
trait Slots {
    /// Chain, node, amount and holder of each, in the order served.
    fn slots(&self) -> Vec<(i64, B256, U256, Option<Address>)>;
}

impl Slots for [EscrowAmount] {
    fn slots(&self) -> Vec<(i64, B256, U256, Option<Address>)> {
        self.iter()
            .map(|a| (a.chain_id, a.handle.handle_node, a.amount, a.holder))
            .collect()
    }
}

impl Slots for AddressAmounts {
    fn slots(&self) -> Vec<(i64, B256, U256, Option<Address>)> {
        self.amounts.slots()
    }
}

/// An anvil chain running the contracts a deployment runs, and the store's
/// rows for it.
struct Chain {
    node: Node,
    libid: Libid,
    store: ChainStore,
}

impl Chain {
    /// Anvil on `id` with the contracts deployed by its first account, and
    /// the store's rows for `id` emptied.
    async fn start(pool: &PgPool, id: u64) -> Self {
        let store = ChainStore::new(pool.clone(), id as i64);
        store.forget().await;
        let node = Node::spawn(id);
        let libid = Libid::deploy(&node.provider, node.accounts()[0]).await;
        Self { node, libid, store }
    }

    /// Runs the indexer's own loop until it committed every block mined so
    /// far, then stops it. Three blocks a window, so a run spans several.
    async fn index(&self, pool: &PgPool) {
        let provider = &self.node.provider;
        let latest = provider.get_block_number().await.expect("latest");
        provider.anvil_mine(Some(1), None).await.expect("mine");
        let contracts = self.libid.contracts();
        let mut lease = self.store.acquire_writer().await.expect("writer lease");
        let writer = self
            .store
            .prepare(&mut lease, contracts)
            .await
            .expect("prepare");
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
                self.node.rpc().try_into().expect("anvil serves http"),
                config,
            )
            .run(cancel.clone()),
        );
        ChainWatch::new(pool, &self.store)
            .await
            .cursor_at(latest)
            .await;
        cancel.cancel();
        task.await
            .expect("the loop ends")
            .expect("the loop ends cleanly");
        lease.release().await.expect("the lease releases");
    }
}

/// The two pages a referrer reads, read across every chain the store holds
/// and kept to the journey's chains: other suites' anvils run the same
/// accounts on chains of their own.
struct Pages<'a> {
    pool: &'a PgPool,
    holder: Address,
    chains: [i64; 2],
}

impl Pages<'_> {
    /// The referral history: the binds that named the holder's handles.
    async fn referrals(&self) -> AddressReferrals {
        let path = format!("/v1/referrals/address/{}", self.holder);
        let mut answer: AddressReferrals = self.pool.get(&path).await.answer();
        answer
            .referrals
            .retain(|r| self.chains.contains(&r.chain_id));
        answer
    }

    /// The referral history: every event the holder took part in.
    async fn history(&self) -> AddressHistory {
        let path = format!("/v1/history/address/{}", self.holder);
        let mut answer: AddressHistory = self.pool.get(&path).await.answer();
        answer.entries.retain(|e| self.chains.contains(&e.chain_id));
        answer
    }

    /// The profile: what `claim` pays the holder now.
    async fn claimable(&self) -> AddressAmounts {
        self.amounts("claimable").await
    }

    /// The profile: what binding its handles elsewhere makes claimable.
    async fn awaiting(&self) -> AddressAmounts {
        self.amounts("awaiting").await
    }

    /// One of the profile's lists, kept to the journey's chains.
    async fn amounts(&self, list: &str) -> AddressAmounts {
        let path = format!("/v1/escrow/{list}/{}", self.holder);
        let mut answer: AddressAmounts = self.pool.get(&path).await.answer();
        answer.amounts.retain(|a| self.chains.contains(&a.chain_id));
        answer
    }
}

#[tokio::test]
async fn indexes_the_splitters_referrals_and_reconciles_them() {
    let chain_id = CHAIN;
    let url = database_url();

    let pool = db::connect_and_migrate(&url).await.expect("database");
    let seeded = ChainStore::new(pool.clone(), SEEDED_CHAIN);
    seeded.forget().await;

    let mut chain = Chain::start(&pool, chain_id).await;
    let &[_, alice, bob, carol, dave, erin, frank, ..] = chain.node.accounts() else {
        panic!("anvil has fewer than seven accounts");
    };
    let (x, google) = (KnownPlatform::X, KnownPlatform::Google);
    let fee = U256::from(BIND_FEE);
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
    chain
        .libid
        .bind(alice, proof(alice_r, "111"), Disclose::Name, U256::ZERO)
        .await;
    chain
        .libid
        .bind(erin, proof(erin_r, "555"), Disclose::Name, U256::ZERO)
        .await;
    chain.libid.refuse_payments(erin).await;

    // Bob's share is forwarded to Alice; Carol's is held for @nobody_a, whom
    // nobody holds here; Alice referring herself and Dave referring Erin pay
    // the treasury the whole fee.
    let treasury_before = chain.libid.balance(TREASURY).await;
    chain
        .libid
        .bind_referred(bob, proof(Handle(x, "bob_a"), "222"), alice_r, fee)
        .await;
    chain
        .libid
        .bind_referred(carol, proof(Handle(x, "carol_a"), "333"), nobody_r, fee)
        .await;
    chain
        .libid
        .bind_referred(
            alice,
            proof(Handle(google, "alice.a@gmail.com"), "123456789012345678901"),
            alice_r,
            fee,
        )
        .await;
    chain
        .libid
        .bind_referred(dave, proof(Handle(x, "dave_a"), "444"), erin_r, fee)
        .await;

    // The owner doubles the price after the binds that paid the old one.
    let price = fee * U256::from(2u64);
    chain.libid.set_bind_fee(price).await;

    // Frank holds @nobody_a on another chain the store holds.
    seeded.bind("nobody_a", frank).await;

    chain
        .libid
        .contracts()
        .check_referrals(&chain.node.provider)
        .await
        .expect("the splitter pays through the escrow of the registry");
    chain.index(&pool).await;
    let libid = &chain.libid;

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
    let nobody_node = libid.node(nobody_r).await;
    assert_eq!(
        awaiting.slots(),
        [(chain_id as i64, nobody_node, escrowed, None)]
    );
    // The holder's handle names the platform the node alone does not.
    assert_eq!(awaiting.amounts[0].handle.platform, Some(x));
    let held: HandleAmounts = pool
        .get_on(chain_id as i64, &format!("/v1/escrow/node/{nobody_node}"))
        .await
        .answer();
    assert_eq!(held.amounts.slots(), awaiting.slots());

    // The money reconciles: the treasury took every fee less the two shares
    // the escrow took, and the splitter keeps nothing.
    let treasury = libid.balance(TREASURY).await - treasury_before;
    assert_eq!(
        treasury,
        fee * U256::from(4u64) - share() * U256::from(2u64)
    );
    assert_eq!(libid.balance(*libid.splitter.address()).await, U256::ZERO);

    // The status shows the price the splitter charges now, from its last
    // `FeeSet`.
    let status: Status = pool.get("/v1/status").await.answer();
    let indexed = status
        .chains
        .iter()
        .find(|c| c.chain_id == chain_id as i64)
        .expect("the indexed chain");
    assert_eq!(indexed.bind_fee, Some(price));
    assert_eq!(libid.splitter.fee().call().await.expect("fee()"), price);
}

/// A splitter is followed only beside the escrow it pays through, of the
/// registry the indexer follows.
#[tokio::test]
async fn a_splitter_of_other_contracts_or_without_an_escrow_is_refused() {
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

/// A referrer's two pages across two chains. The referral history is
/// `/v1/referrals` and `/v1/history`; the profile is `/v1/escrow/claimable`
/// and `/v1/escrow/awaiting`. R holds `@referrer_r` on chain A. B1 binds on
/// chain B naming it, where nobody holds it, so the escrow holds R's share
/// there; B2 binds on chain A naming it, so the splitter forwards the share
/// to R. R then binds the handle on B and claims what it holds.
#[tokio::test]
async fn a_referral_across_two_chains_reads_on_both_pages() {
    let pool = db::connect_and_migrate(&database_url())
        .await
        .expect("database");
    let mut a = Chain::start(&pool, CHAIN_A).await;
    let mut b = Chain::start(&pool, CHAIN_B).await;
    let &[_, r, b1, b2, ..] = a.node.accounts() else {
        panic!("anvil has fewer than four accounts");
    };
    let (on_a, on_b) = (CHAIN_A as i64, CHAIN_B as i64);
    let x = KnownPlatform::X;
    let fee = U256::from(BIND_FEE);
    let proof = |handle, id| Proof {
        handle,
        id,
        observed_at: 1000,
    };
    let referrer = Handle(x, "referrer_r");
    let node = a.libid.node(referrer).await;
    assert_eq!(
        b.libid.node(referrer).await,
        node,
        "one node on every chain"
    );
    let pages = Pages {
        pool: &pool,
        holder: r,
        chains: [on_a, on_b],
    };

    // R holds @referrer_r on A; B knows the contracts and nothing else.
    a.libid
        .bind(r, proof(referrer, "777"), Disclose::Name, U256::ZERO)
        .await;
    a.index(&pool).await;
    b.index(&pool).await;

    // B1 binds on B naming it: nobody holds it there, so the escrow holds
    // R's share, awaiting R's bind on B.
    let bind = b
        .libid
        .referred(b1, proof(Handle(x, "binder_b1"), "801"), referrer, fee)
        .await;
    let bound_b1 = b.node.provider.mined(bind).await.transaction_hash;
    b.index(&pool).await;
    assert_eq!(b.libid.escrowed(referrer).await, share());
    assert_eq!(
        pages.referrals().await.on_chains(),
        [(on_b, b1, Outcome::Paid, Some(Delivery::Held))]
    );
    assert_eq!(
        pages.awaiting().await.slots(),
        [(on_b, node, share(), None)]
    );
    assert_eq!(pages.claimable().await.slots(), []);

    // B2 binds on A naming it: the splitter forwards the share to R.
    let before = a.libid.balance(r).await;
    let bind = a
        .libid
        .referred(b2, proof(Handle(x, "binder_b2"), "802"), referrer, fee)
        .await;
    let bound_b2 = a.node.provider.mined(bind).await.transaction_hash;
    a.index(&pool).await;
    assert_eq!(a.libid.balance(r).await - before, share());
    // Each referral names the transaction that bound, for an explorer link.
    assert_eq!(
        pages.referrals().await.transactions(),
        [(on_a, bound_b2), (on_b, bound_b1)]
    );
    assert_eq!(
        pages.referrals().await.on_chains(),
        [
            (on_a, b2, Outcome::Paid, Some(Delivery::Forwarded)),
            (on_b, b1, Outcome::Paid, Some(Delivery::Held)),
        ]
    );

    // R binds it on B: the held share is claimable there.
    b.libid
        .bind(r, proof(referrer, "777"), Disclose::Name, U256::ZERO)
        .await;
    b.index(&pool).await;
    assert_eq!(
        pages.claimable().await.slots(),
        [(on_b, node, share(), Some(r))]
    );
    assert_eq!(pages.awaiting().await.slots(), []);

    // R claims on B: the share reads claimed, nothing waits on either page,
    // and R's history shows the forward on A and the claim on B.
    b.libid.claim(r, referrer).await;
    b.index(&pool).await;
    assert_eq!(b.libid.escrowed(referrer).await, U256::ZERO);
    assert_eq!(
        pages.referrals().await.on_chains(),
        [
            (on_a, b2, Outcome::Paid, Some(Delivery::Forwarded)),
            (on_b, b1, Outcome::Paid, Some(Delivery::Claimed)),
        ]
    );
    assert_eq!(pages.claimable().await.slots(), []);
    assert_eq!(pages.awaiting().await.slots(), []);
    let splitter = *a.libid.splitter.address();
    let paid: Vec<_> = pages
        .history()
        .await
        .entries
        .iter()
        .filter_map(|entry| match entry.event {
            HistoryEvent::Forwarded {
                sender,
                holder,
                amount,
                ..
            } => Some((entry.chain_id, sender, holder, amount)),
            HistoryEvent::Claimed {
                claimer,
                recipient,
                released,
                ..
            } => Some((entry.chain_id, claimer, recipient, released)),
            _ => None,
        })
        .collect();
    assert_eq!(
        paid,
        [(on_b, r, r, share()), (on_a, splitter, r, share())],
        "newest first: the claim on B, then the forward on A"
    );
}
