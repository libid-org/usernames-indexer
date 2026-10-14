//! End to end against a real chain: anvil runs, mock contracts with the exact
//! IdentityRegistry and HandleEscrow event surfaces emit a scenario, and the
//! indexer's own loop — deployment-block detection included — indexes both
//! into Postgres, where the API answers.
//!
//! Skips silently in exactly two cases: `DATABASE_URL` unset, or no `anvil`
//! binary on PATH. Everything past those checks panics on failure.

use std::{
    process::Command,
    time::Duration,
};

use alloy::{
    primitives::{
        address,
        Address,
        B256,
        U256,
    },
    providers::{
        ext::AnvilApi,
        Provider,
        ProviderBuilder,
    },
};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        AddressAmounts,
        AddressHistory,
        AddressResolution,
        HandleHistory,
        HandleResolution,
        HistoryEvent,
        IdResolution,
        Role,
        SearchResults,
        Status,
    },
    chain,
    db::{
        self,
        ChainStore,
    },
    indexer,
    nodes::KnownPlatform,
};

mod common;
use common::{
    events,
    MockHandleEscrow,
    MockIdentityRegistry,
    Reply,
};

/// A request scoped to this suite's chain: the store is shared with the
/// read-model suite's chain.
async fn get<T: DeserializeOwned>(pool: &PgPool, path: &str) -> Reply<T> {
    common::get_on(pool, CHAIN as i64, path).await
}

/// Not 31337: the read-model tests use anvil's default id against the same
/// database, and chain_id keying is exactly the isolation this exercises.
const CHAIN: u64 = 43117;

/// A Google account id as the platform issues it: the OIDC `sub`.
const GOOGLE_USER_ID: &str = "123456789012345678901";

/// A bind of `id` that discloses `handle`, with nothing to withdraw or retire.
fn disclosing_bind(
    holder: Address,
    platform: KnownPlatform,
    id: &str,
    handle: &str,
    observed_at: u64,
    digest: B256,
) -> MockIdentityRegistry::Bind {
    common::mock_bind(holder, platform, id, handle, observed_at, true, digest)
}

#[tokio::test]
async fn indexes_a_real_chain_end_to_end() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if Command::new("anvil").arg("--version").output().is_err() {
        eprintln!("skipping: anvil not on PATH");
        return;
    }

    let pool = db::connect_and_migrate(&url).await.expect("database");
    let store = ChainStore::new(pool.clone(), CHAIN as i64);
    // A previous run of this test left rows under this chain id; the loop
    // must start from a clean slate to make block-number assertions exact.
    common::clear_chain(&pool, CHAIN as i64).await;

    let provider = ProviderBuilder::new()
        .connect_anvil_with_wallet_and_config(|anvil| anvil.chain_id(CHAIN))
        .expect("anvil spawns");

    // Blocks before the deploy give the binary search something to find.
    provider.anvil_mine(Some(5), None).await.expect("mine");

    let mock = MockIdentityRegistry::deploy(provider.clone())
        .await
        .expect("deploy");
    let deploy_block = provider.get_block_number().await.expect("block");

    let (x, google) = (KnownPlatform::X, KnownPlatform::Google);
    let alice = Address::repeat_byte(0xA1);
    let verifier = Address::repeat_byte(0xEE);
    let fee_receiver = Address::repeat_byte(0xFE);
    let digest = B256::repeat_byte(0xD1);

    // The scenario, one tx per step (anvil automines a block each): wire the
    // Proof Verifier, bind disclosing the handle — the ceremony's own events
    // beside it — rename, add a disclosed Google identity, withdraw its name.
    mock.emitProofVerifierConfigured(verifier)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitBind(disclosing_bind(alice, x, "111", "alice_1", 1000, digest))
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitBindFeePaid(digest, fee_receiver, U256::from(5u64))
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitHandleRetired(x.id(), x.handle_node("alice_1").unwrap(), alice)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    let digest_2 = B256::repeat_byte(0xD2);
    mock.emitBind(disclosing_bind(alice, x, "111", "alice_2", 2000, digest_2))
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    let google_bind = disclosing_bind(
        alice,
        google,
        GOOGLE_USER_ID,
        "a.b+tag@example.com",
        3000,
        B256::repeat_byte(0xD3),
    );
    mock.emitBind(google_bind)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitHandleUnpublished(alice, google.id())
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();

    // The escrow, deployed after the registry it resolves through: a payer
    // escrows for @bob_x, whom nobody holds yet, and pays Alice's held handle
    // straight through; Bob then binds @bob_x and claims what waited.
    let escrow = MockHandleEscrow::deploy(provider.clone(), *mock.address())
        .await
        .expect("deploy the escrow");
    chain::check_escrow(&provider, *escrow.address(), *mock.address())
        .await
        .expect("the escrow resolves holders through the registry");
    chain::check_registry(&provider, *mock.address())
        .await
        .expect("the registry is keyed by nodes");
    // The escrow has no `nodeKeyed`: an indexer pointed at it refuses.
    assert!(matches!(
        chain::check_registry(&provider, *escrow.address()).await,
        Err(chain::RegistryRefused::Unread { .. })
    ));
    let native = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
    let (payer, bob) = (Address::repeat_byte(0xC0), Address::repeat_byte(0xB0));
    let bob_node = x.handle_node("bob_x").unwrap();
    escrow
        .emitDeposited(
            bob_node,
            native,
            payer,
            payer,
            U256::ZERO,
            U256::from(100u64),
        )
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    escrow
        .emitForwarded(
            x.handle_node("alice_2").unwrap(),
            native,
            payer,
            alice,
            U256::from(7u64),
            U256::from(7u64),
        )
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitIdentityBound(bob, x.id_node("222").unwrap(), bob_node, x.id(), 4000, 1)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    escrow
        .emitClaimed(
            bob_node,
            native,
            bob,
            bob,
            U256::ZERO,
            U256::from(100u64),
            U256::from(100u64),
        )
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();

    let latest = provider.get_block_number().await.expect("latest");

    // What the binary does before its loop: take the chain's writer lease and
    // prepare the chain, which records the contract the rows come from.
    let writer = store.acquire_writer().await.expect("writer lease");
    store
        .prepare(&writer, *mock.address(), Some(*escrow.address()))
        .await
        .expect("prepare");

    // Run the real loop: no start override (detection must find the deploy
    // block), a tiny window so catch-up spans several chunks, no
    // confirmation lag on a chain that cannot reorg.
    let cancel = CancellationToken::new();
    let config = indexer::IndexerConfig {
        contract: *mock.address(),
        escrow: Some(*escrow.address()),
        confirmations: 0,
        poll_interval: Duration::from_millis(20),
        max_block_range: 2,
        start_block: None,
        // A report good for two minutes; the loop renews it every cycle.
        stale_after: Duration::from_secs(120),
    };
    let task = tokio::spawn(
        indexer::Indexer::new(store.clone(), provider.clone(), config)
            .run(cancel.clone()),
    );

    common::wait_for_cursor(&store, latest).await;
    cancel.cancel();
    let _ = task.await;
    writer.release().await.expect("the lease releases");

    // Detection ran once and its answer was cached — and it is the right one.
    let cached = store.deploy_block(*mock.address()).await.expect("metadata");
    assert_eq!(cached, Some(deploy_block), "cached deployment block");

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
    assert_eq!(binding.ceremony_version, 1);
    assert_eq!(binding.handle.as_deref(), Some("alice_2"));
    let resolved: IdResolution = get(
        &pool,
        &format!("/v1/resolve/id-node/{}", x.id_node("111").unwrap()),
    )
    .await
    .answer();
    assert_eq!(resolved.bindings[0].handle.as_deref(), Some("alice_2"));

    // The Google identity resolves through the URL-encoded raw form; its
    // name was withdrawn, so no text comes back.
    let resolved: HandleResolution =
        get(&pool, "/v1/resolve/handle/google/A.B%2Btag%40Example.COM")
            .await
            .answer();
    assert_eq!(
        resolved.handle_node,
        google.handle_node("a.b+tag@example.com").unwrap()
    );
    assert_eq!(resolved.bindings[0].handle, None);
    assert_eq!(
        resolved.bindings[0].id_node,
        google.id_node(GOOGLE_USER_ID).unwrap()
    );
    let resolved: IdResolution = get(
        &pool,
        &format!(
            "/v1/resolve/id-node/{}",
            google.id_node(GOOGLE_USER_ID).unwrap()
        ),
    )
    .await
    .answer();
    assert_eq!(resolved.bindings[0].owner, alice);
    assert_eq!(resolved.bindings[0].handle, None);

    // Reverse: both identities; X's name is displayed, Google's was
    // withdrawn.
    let resolved: AddressResolution = get(&pool, &format!("/v1/resolve/address/{alice}"))
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
        [(Some(x), true), (Some(google), false)],
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
    assert_eq!(chain.contract, Some(*mock.address()));
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
    assert_eq!(chain.proof_verifier, Some(verifier));
    assert_eq!(chain.escrow, Some(*escrow.address()));
    let platform_key: Option<String> = sqlx::query_scalar(
        "SELECT platform_key FROM names.platforms WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(CHAIN as i64)
    .bind(x.id().as_slice())
    .fetch_one(&pool)
    .await
    .expect("platform row");
    assert_eq!(platform_key.as_deref(), Some("x"));

    // The journal holds every event the scenario emitted, each bind's
    // ceremony and the fee included.
    let journal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.events WHERE chain_id = $1")
            .bind(CHAIN as i64)
            .fetch_one(&pool)
            .await
            .expect("journal count");
    assert_eq!(journal, 17);
    let fee_kinds: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM names.events WHERE chain_id = $1 AND kind = 'bind_fee_paid'",
    )
    .bind(CHAIN as i64)
    .fetch_one(&pool)
    .await
    .expect("fee count");
    assert_eq!(fee_kinds, 1);

    // Both emitters' logs, in one history: the handle's from the deposit made
    // while nobody held it through the bind to the claim, each dated by its
    // block.
    let history: HandleHistory = get(&pool, "/v1/history/handle/x/bob_x").await.answer();
    assert!(
        matches!(
            events(&history.entries)[..],
            [
                HistoryEvent::Claimed { .. },
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
                HistoryEvent::Forwarded { .. },
                HistoryEvent::Deposited { .. },
            ]
        ),
        "{history:?}"
    );
    assert_eq!(history.entries[0].roles, [Role::RefundTo]);
    assert_eq!(history.entries[2].roles, [Role::Depositor, Role::RefundTo]);
    let history: AddressHistory = get(&pool, &format!("/v1/history/address/{alice}"))
        .await
        .answer();
    assert_eq!(history.entries[0].roles, [Role::Holder]);
    assert!(matches!(
        history.entries[0].event,
        HistoryEvent::Forwarded { received, .. } if received == U256::from(7u64)
    ));

    // Claimed: nothing waits for Bob, and the payer's refund ended with it.
    let bobs: AddressAmounts = get(&pool, &format!("/v1/escrow/claimable/{bob}"))
        .await
        .answer();
    assert!(bobs.amounts.is_empty(), "{bobs:?}");
    let payers: AddressAmounts = get(&pool, &format!("/v1/escrow/refundable/{payer}"))
        .await
        .answer();
    assert!(payers.amounts.is_empty(), "{payers:?}");
}

#[tokio::test]
async fn an_escrow_of_another_registry_is_refused() {
    if Command::new("anvil").arg("--version").output().is_err() {
        eprintln!("skipping: anvil not on PATH");
        return;
    }
    let provider = ProviderBuilder::new()
        .connect_anvil_with_wallet_and_config(|anvil| anvil.chain_id(CHAIN))
        .expect("anvil spawns");
    let (indexed, other) = (Address::repeat_byte(0x11), Address::repeat_byte(0x22));
    let escrow = MockHandleEscrow::deploy(provider.clone(), other)
        .await
        .expect("deploy the escrow");

    let refused = chain::check_escrow(&provider, *escrow.address(), indexed).await;
    assert!(
        matches!(
            refused,
            Err(chain::EscrowRefused::OtherRegistry { registry, indexed: asked, .. })
                if registry == other && asked == indexed
        ),
        "{refused:?}"
    );
}
