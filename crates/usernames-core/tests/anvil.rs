//! End to end against a real chain: anvil runs the libid contracts a
//! deployment runs, with a stub in place of each platform's proof check (see
//! `common::libid`). The indexer's own loop, deployment-block detection
//! included, indexes the registry and the escrow into Postgres, where the API
//! answers. The scenario runs once per log source, each on a chain of its own.
//!
//! Skips without `DATABASE_URL` or `anvil`, unless `CI` is set: there a
//! missing one fails. Everything past those checks panics on failure.

use std::time::Duration;

use alloy::{
    primitives::U256,
    providers::{
        ext::AnvilApi,
        Provider,
    },
};
use axum::http::StatusCode;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressContributions,
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
    chain::{
        self,
        Contracts,
    },
    db::{
        self,
        ChainStore,
    },
    events::EventKind,
    indexer::{
        self,
        LogSource,
    },
    nodes::KnownPlatform,
};

mod common;
use common::{
    libid::{
        Disclose,
        Handle,
        Libid,
        Node,
        Proof,
        CEREMONY_VERSION,
    },
    Entries,
    IdNode,
    Reads,
};

/// The polled chain. Not 31337: the read-model tests use anvil's default id
/// against the same database, and chain_id keying is the isolation this
/// exercises.
const POLLED: u64 = 43117;

/// The subscribed chain, apart from the polled one so both run at once.
const SUBSCRIBED: u64 = 43119;

/// A Google account id as the platform issues it: the OIDC `sub`.
const GOOGLE_USER_ID: &str = "123456789012345678901";

#[tokio::test]
async fn indexes_a_real_chain_end_to_end_by_polling() {
    indexes_a_real_chain_end_to_end(LogSource::Poll, POLLED).await;
}

#[tokio::test]
async fn indexes_a_real_chain_end_to_end_from_a_subscription() {
    indexes_a_real_chain_end_to_end(LogSource::Subscribe, SUBSCRIBED).await;
}

/// The scenario, sent before the indexer starts and indexed through
/// `source`: deployment detection, catch-up and the API's answers.
async fn indexes_a_real_chain_end_to_end(source: LogSource, chain_id: u64) {
    let Some(url) = common::database_url() else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !common::anvil_available() {
        eprintln!("skipping: anvil not on PATH");
        return;
    }

    let pool = db::connect_and_migrate(&url).await.expect("database");
    let store = ChainStore::new(pool.clone(), chain_id as i64);
    // A previous run of this test left rows under this chain id; the loop
    // must start from a clean slate to make block-number assertions exact.
    ChainStore::new(pool.clone(), chain_id as i64)
        .forget()
        .await;

    let node = Node::spawn(chain_id);
    let provider = &node.provider;
    let &[owner, alice, bob, payer, refunder, ..] = node.accounts() else {
        panic!("anvil has fewer than five accounts");
    };

    // Blocks before the deploy give the binary search something to find.
    provider.anvil_mine(Some(5), None).await.expect("mine");
    let mut libid = Libid::deploy(provider, owner).await;
    let (x, google) = (KnownPlatform::X, KnownPlatform::Google);

    // Alice binds @alice_1 as her name and pays the app's fee, renames to
    // @alice_2, binds a Google address whose name replaces the X one, and
    // withdraws that name.
    let alice_1 = Proof {
        handle: Handle(x, "alice_1"),
        id: "111",
        observed_at: 1000,
    };
    libid
        .bind(alice, alice_1, Disclose::Name, U256::from(5u64))
        .await;
    let alice_2 = Proof {
        handle: Handle(x, "alice_2"),
        id: "111",
        observed_at: 2000,
    };
    libid.bind(alice, alice_2, Disclose::Name, U256::ZERO).await;
    let alice_google = Proof {
        handle: Handle(google, "A.B+tag@Example.COM"),
        id: GOOGLE_USER_ID,
        observed_at: 3000,
    };
    libid
        .bind(alice, alice_google, Disclose::Name, U256::ZERO)
        .await;
    libid.unpublish(alice).await;

    // A payer escrows for @bob_x, whom nobody holds yet, pays Alice's held
    // @alice_2 straight through, and escrows for @carol_x. A second payer
    // escrows for @carol_x, takes it back once its refund date has come, and
    // escrows again. Bob then binds @bob_x privately and claims what waited;
    // nobody binds @carol_x.
    let (bob_x, carol_x) = (Handle(x, "bob_x"), Handle(x, "carol_x"));
    libid.deposit(payer, bob_x, 100).await;
    libid.deposit(payer, Handle(x, "alice_2"), 7).await;
    libid.deposit(payer, carol_x, 30).await;
    libid.deposit(refunder, carol_x, 20).await;
    libid.pass_refund_grace().await;
    libid.refund(refunder, carol_x).await;
    libid.deposit(refunder, carol_x, 15).await;
    let bob_proof = Proof {
        handle: bob_x,
        id: "222",
        observed_at: 4000,
    };
    libid
        .bind(bob, bob_proof, Disclose::Nothing, U256::ZERO)
        .await;
    libid.claim(bob, bob_x).await;

    let latest = provider.get_block_number().await.expect("latest");
    // One block on top, so the scenario's last event is a confirmation deep.
    provider.anvil_mine(Some(1), None).await.expect("mine");
    let (registry, escrow) = (*libid.registry.address(), *libid.escrow.address());

    // What the binary does before its loop: check both contracts, take the
    // chain's writer lease and prepare the chain, which records the contracts
    // the rows come from.
    let contracts = Contracts {
        registry,
        escrow: Some(escrow),
    };
    contracts
        .check_escrow(provider)
        .await
        .expect("the escrow resolves holders through the registry");
    contracts
        .check_registry(provider)
        .await
        .expect("the registry is keyed by nodes");
    // The escrow has no `nodeKeyed`: an indexer pointed at it refuses.
    let escrow_as_registry = Contracts {
        registry: escrow,
        escrow: None,
    };
    assert!(matches!(
        escrow_as_registry.check_registry(provider).await,
        Err(chain::RegistryRefused::Unread { .. })
    ));
    let lease = store.acquire_writer().await.expect("writer lease");
    let writer = store.prepare(&lease, contracts).await.expect("prepare");

    // Run the real loop: no start override (detection must find the deploy
    // block), a tiny window so catch-up spans several chunks, and the one
    // confirmation a subscription needs.
    let cancel = CancellationToken::new();
    let config = indexer::IndexerConfig {
        contracts,
        confirmations: 1,
        source,
        poll_interval: Duration::from_millis(20),
        max_block_range: 2,
        start_block: None,
        // A report good for two minutes; the loop renews it every cycle.
        stale_after: Duration::from_secs(120),
    };
    let task = tokio::spawn(
        indexer::Indexer::new(writer, node.rpc(), config).run(cancel.clone()),
    );

    common::ChainWatch::new(&pool, &store)
        .await
        .cursor_at(latest)
        .await;
    cancel.cancel();
    let _ = task.await;
    lease.release().await.expect("the lease releases");

    // Detection ran once and its answer was cached — and it is the right one.
    let cached = store.deploy_block(registry).await.expect("metadata");
    assert_eq!(cached, Some(libid.deployed_at), "cached deployment block");

    // The indexer files a handle under the node the registry does.
    assert_eq!(x.handle_node("bob_x").unwrap(), libid.node(bob_x).await);

    // alice_1 retired, alice_2 resolves, and the id followed the rename.
    let (status, _) = pool
        .get_on::<HandleResolution>(chain_id as i64, "/v1/resolve/handle/x/alice_1")
        .await
        .refusal();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let resolved: HandleResolution = pool
        .get_on(chain_id as i64, "/v1/resolve/handle/x/alice_2")
        .await
        .answer();
    let binding = &resolved.bindings[0];
    assert_eq!(binding.chain_id, chain_id as i64);
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.ceremony_version, i64::from(CEREMONY_VERSION));
    assert_eq!(binding.handle.as_deref(), Some("alice_2"));
    let resolved: IdResolution = pool
        .get_on(
            chain_id as i64,
            &format!("/v1/resolve/id-node/{}", x.id_node("111")),
        )
        .await
        .answer();
    assert_eq!(resolved.bindings[0].handle.as_deref(), Some("alice_2"));

    // The registry folded the Google address it was given; the API finds it
    // through the URL-encoded raw form, and the text it disclosed still comes
    // back after the name was withdrawn.
    let resolved: HandleResolution = pool
        .get_on(
            chain_id as i64,
            "/v1/resolve/handle/google/A.B%2Btag%40Example.COM",
        )
        .await
        .answer();
    assert_eq!(
        resolved.handle_node,
        google.handle_node("a.b+tag@example.com").unwrap()
    );
    assert_eq!(
        resolved.bindings[0].handle.as_deref(),
        Some("a.b+tag@example.com")
    );
    assert_eq!(resolved.bindings[0].id_node, google.id_node(GOOGLE_USER_ID));
    let resolved: IdResolution = pool
        .get_on(
            chain_id as i64,
            &format!("/v1/resolve/id-node/{}", google.id_node(GOOGLE_USER_ID)),
        )
        .await
        .answer();
    assert_eq!(resolved.bindings[0].owner, alice);
    assert_eq!(
        resolved.bindings[0].handle.as_deref(),
        Some("a.b+tag@example.com")
    );

    // Reverse: both identities, neither the name: Google's replaced X's, and
    // was withdrawn.
    let resolved: AddressResolution = pool
        .get_on(chain_id as i64, &format!("/v1/resolve/address/{alice}"))
        .await
        .answer();
    assert_eq!(resolved.identities.len(), 2, "{resolved:?}");
    // Platform ids order the identities: X's sorts before Google's.
    let displayed: Vec<_> = resolved
        .identities
        .iter()
        .map(|i| (i.platform, i.published))
        .collect();
    assert_eq!(
        displayed,
        [(Some(x), false), (Some(google), false)],
        "{resolved:?}"
    );
    assert!(
        resolved.identities.iter().all(|i| i.resolves),
        "{resolved:?}"
    );

    // Search sees the current handle, not the retired one.
    let results: SearchResults = pool
        .get_on(chain_id as i64, "/v1/search?q=alice&platform=x")
        .await
        .answer();
    let handles: Vec<&str> = results.hits.iter().map(|h| h.handle.as_str()).collect();
    assert_eq!(handles, ["alice_2"], "{results:?}");

    // Ops metadata filled from the admin events, on this chain's entry of
    // the status.
    let status: Status = pool.get("/v1/status").await.answer();
    let chain = status
        .chains
        .iter()
        .find(|c| c.chain_id == chain_id as i64)
        .unwrap_or_else(|| panic!("chain {chain_id} is listed: {status:?}"));
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

    // The journal holds every event the registry and the escrow emitted: the
    // Proof Verifier configured; four binds, each with its ceremony, the
    // disclosing ones with their reveal and name, the first with its fee and
    // the rename with its retirement and withdrawal; the withdrawal; six
    // escrow moves and the claim.
    let journal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.events WHERE chain_id = $1")
            .bind(chain_id as i64)
            .fetch_one(&pool)
            .await
            .expect("journal count");
    assert_eq!(journal, 26);
    let fee_kinds: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM names.events WHERE chain_id = $1 AND kind = $2",
    )
    .bind(chain_id as i64)
    .bind(EventKind::BindFeePaid.as_str())
    .fetch_one(&pool)
    .await
    .expect("fee count");
    assert_eq!(fee_kinds, 1);

    // Both emitters' logs, in one history: the handle's from the deposit made
    // while nobody held it, through the bind and its ceremony, to the claim,
    // each dated by its block.
    let history: HandleHistory = pool
        .get_on(chain_id as i64, "/v1/history/handle/x/bob_x")
        .await
        .answer();
    assert!(
        matches!(
            history.entries.events()[..],
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
    let history: AddressHistory = pool
        .get_on(chain_id as i64, &format!("/v1/history/address/{payer}"))
        .await
        .answer();
    assert!(
        matches!(
            history.entries.events()[..],
            [
                HistoryEvent::Claimed { .. },
                HistoryEvent::Deposited { .. },
                HistoryEvent::Forwarded { .. },
                HistoryEvent::Deposited { .. },
            ]
        ),
        "{history:?}"
    );
    assert_eq!(history.entries[0].roles, [Role::Sender]);
    assert_eq!(history.entries[3].roles, [Role::Sender]);
    let history: AddressHistory = pool
        .get_on(chain_id as i64, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Holder]);
    assert!(matches!(
        history.entries[0].event,
        HistoryEvent::Forwarded { received, .. } if received == U256::from(7u64)
    ));
    // The app's fee is in its payer's history, as the holder of the handle it
    // bought.
    let fee = history
        .entries
        .iter()
        .find(|entry| matches!(entry.event, HistoryEvent::BindFeePaid { .. }))
        .expect("the fee is in Alice's history");
    assert_eq!(fee.roles, [Role::Holder]);
    assert_eq!(
        fee.handle
            .as_ref()
            .and_then(|handle| handle.handle.as_deref()),
        Some("alice_1")
    );
    let history: AddressHistory = pool
        .get_on(chain_id as i64, &format!("/v1/history/address/{refunder}"))
        .await
        .answer();
    assert!(
        matches!(
            history.entries.events()[..],
            [
                HistoryEvent::Deposited { .. },
                HistoryEvent::Refunded { .. },
                HistoryEvent::Deposited { .. },
            ]
        ),
        "{history:?}"
    );

    // The books match the escrow's own: Bob's slot is empty, and what waits
    // for @carol_x is what each payer could take back, from the date the
    // escrow set.
    let bobs: AddressAmounts = pool
        .get_on(chain_id as i64, &format!("/v1/escrow/claimable/{bob}"))
        .await
        .answer();
    assert!(bobs.amounts.is_empty(), "{bobs:?}");
    assert_eq!(libid.escrowed(bob_x).await, U256::ZERO);
    let carols: HandleAmounts = pool
        .get_on(chain_id as i64, "/v1/escrow/handle/x/carol_x")
        .await
        .answer();
    assert_eq!(carols.amounts.len(), 1, "{carols:?}");
    assert_eq!(carols.amounts[0].amount, libid.escrowed(carol_x).await);
    assert_eq!(carols.amounts[0].amount, U256::from(45u64));
    let unclaimed: Unclaimed = pool
        .get_on(chain_id as i64, "/v1/escrow/unclaimed")
        .await
        .answer();
    assert_eq!(unclaimed.amounts, carols.amounts);
    for (sender, put_in) in [(payer, 30u64), (refunder, 15)] {
        let refundable: AddressContributions = pool
            .get_on(chain_id as i64, &format!("/v1/escrow/refundable/{sender}"))
            .await
            .answer();
        assert_eq!(refundable.amounts.len(), 1, "{refundable:?}");
        let served = &refundable.amounts[0];
        let (booked, refundable_at) = libid.contribution(carol_x, sender).await;
        assert_eq!(served.amount.amount, booked);
        assert_eq!(served.amount.amount, U256::from(put_in));
        assert_eq!(
            u64::try_from(served.refundable_at).ok(),
            Some(refundable_at)
        );
    }
}

#[tokio::test]
async fn an_escrow_of_another_registry_is_refused() {
    if !common::anvil_available() {
        eprintln!("skipping: anvil not on PATH");
        return;
    }
    let node = Node::spawn(POLLED);
    let (provider, owner) = (&node.provider, node.accounts()[0]);
    let indexed = Libid::registry(provider, owner).await;
    let other = Libid::registry(provider, owner).await;
    let escrow = Libid::escrow(provider, owner, other).await;

    let refused = Contracts {
        registry: indexed,
        escrow: Some(escrow),
    }
    .check_escrow(provider)
    .await;
    assert!(
        matches!(
            refused,
            Err(chain::EscrowRefused::OtherRegistry { registry, indexed: asked, .. })
                if registry == other && asked == indexed
        ),
        "{refused:?}"
    );
}
