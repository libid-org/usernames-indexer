//! End to end against a real chain: anvil runs, a mock contract with the
//! exact IdentityNames event surface emits a scenario, and the indexer's own
//! loop — deployment-block detection included — indexes it into Postgres,
//! where the API answers.
//!
//! Skips silently in exactly two cases: `DATABASE_URL` unset, or no `anvil`
//! binary on PATH. Everything past those checks panics on failure.

use std::time::Duration;

use alloy::{
    primitives::{
        Address,
        Bytes,
        B256,
        U256,
    },
    providers::{
        ext::AnvilApi,
        Provider,
        ProviderBuilder,
    },
    sol,
};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use usernames_core::{
    api::model::{
        AddressResolution,
        HandleResolution,
        IdResolution,
        SearchResults,
        Status,
    },
    db::{
        self,
        ChainStore,
    },
    indexer,
    nodes,
};

mod common;
use common::Reply;

/// A request scoped to this suite's chain: the store is shared with the
/// read-model suite's chain.
async fn get<T: DeserializeOwned>(store: &ChainStore, path: &str) -> Reply<T> {
    let separator = if path.contains('?') { '&' } else { '?' };
    common::get(store, &format!("{path}{separator}chain={CHAIN}")).await
}

sol! {
    /// The event surface of IdentityNames behind bare emit functions; source
    /// and regeneration notes in contracts/MockIdentityNames.sol.
    /// (The nine-argument emitter mirrors the nine-field event; clippy's
    /// arity taste does not apply to an ABI.)
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    #[sol(bytecode = "0x6080604052348015600e575f5ffd5b50610a268061001c5f395ff3fe608060405234801561000f575f5ffd5b506004361061007b575f3560e01c80638ab9845c116100595780638ab9845c146100d3578063e49087d8146100ef578063ed3435ae1461010b578063f9e5c728146101275761007b565b806347ad7d271461007f57806365186ce61461009b57806370b582b9146100b7575b5f5ffd5b61009960048036038101906100949190610392565b610143565b005b6100b560048036038101906100b09190610417565b610173565b005b6100d160048036038101906100cc9190610442565b6101ad565b005b6100ed60048036038101906100e89190610492565b6101f7565b005b61010960048036038101906101049190610531565b61023f565b005b610125600480360381019061012091906106b3565b610298565b005b610141600480360381019061013c91906107eb565b610303565b005b807fb01c0a30cea93f12bdaa4228f3ced17148be2cc93045c1848d99d588c0141c8f60405160405180910390a250565b7f01970f3cbef68f8ac95615ab5c75299e421a83a787173408d693928ed40b6574816040516101a2919061084a565b60405180910390a150565b8073ffffffffffffffffffffffffffffffffffffffff1682847f278e2122d24782b703f05416ac53a9750d67bcf6fa1603b94ec95660821a891460405160405180910390a4505050565b808273ffffffffffffffffffffffffffffffffffffffff167f8e43ba98be6948052c3368d53fef5505611df0ae5994d278a646eb9ad3479edd60405160405180910390a35050565b828473ffffffffffffffffffffffffffffffffffffffff16867ff0f0b831e902ded46acfd6caf87649edb445c2e2733992b2e79cd1420719c19c85856040516102899291906108bd565b60405180910390a45050505050565b888a8c73ffffffffffffffffffffffffffffffffffffffff167f8ae08d06a548b84d8340ef35ae06cd4c34983cd87b5554e6c818b8180530950c8b8b8b8b8b8b8b8b6040516102ee989796959493929190610957565b60405180910390a45050505050505050505050565b8173ffffffffffffffffffffffffffffffffffffffff16837f471f5d0665e5719861746aa9077ef4997ae3d84cad4f349f5e7cc2cfe74be5998360405161034a91906109d7565b60405180910390a3505050565b5f5ffd5b5f5ffd5b5f819050919050565b6103718161035f565b811461037b575f5ffd5b50565b5f8135905061038c81610368565b92915050565b5f602082840312156103a7576103a6610357565b5b5f6103b48482850161037e565b91505092915050565b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f6103e6826103bd565b9050919050565b6103f6816103dc565b8114610400575f5ffd5b50565b5f81359050610411816103ed565b92915050565b5f6020828403121561042c5761042b610357565b5b5f61043984828501610403565b91505092915050565b5f5f5f6060848603121561045957610458610357565b5b5f6104668682870161037e565b93505060206104778682870161037e565b925050604061048886828701610403565b9150509250925092565b5f5f604083850312156104a8576104a7610357565b5b5f6104b585828601610403565b92505060206104c68582860161037e565b9150509250929050565b5f5ffd5b5f5ffd5b5f5ffd5b5f5f83601f8401126104f1576104f06104d0565b5b8235905067ffffffffffffffff81111561050e5761050d6104d4565b5b60208301915083600182028301111561052a576105296104d8565b5b9250929050565b5f5f5f5f5f6080868803121561054a57610549610357565b5b5f6105578882890161037e565b955050602061056888828901610403565b94505060406105798882890161037e565b935050606086013567ffffffffffffffff81111561059a5761059961035b565b5b6105a6888289016104dc565b92509250509295509295909350565b5f5f83601f8401126105ca576105c96104d0565b5b8235905067ffffffffffffffff8111156105e7576105e66104d4565b5b602083019150836001820283011115610603576106026104d8565b5b9250929050565b5f67ffffffffffffffff82169050919050565b6106268161060a565b8114610630575f5ffd5b50565b5f813590506106418161061d565b92915050565b5f8115159050919050565b61065b81610647565b8114610665575f5ffd5b50565b5f8135905061067681610652565b92915050565b5f61ffff82169050919050565b6106928161067c565b811461069c575f5ffd5b50565b5f813590506106ad81610689565b92915050565b5f5f5f5f5f5f5f5f5f5f5f6101208c8e0312156106d3576106d2610357565b5b5f6106e08e828f01610403565b9b505060206106f18e828f0161037e565b9a505060406107028e828f0161037e565b99505060606107138e828f0161037e565b98505060808c013567ffffffffffffffff8111156107345761073361035b565b5b6107408e828f016105b5565b975097505060a08c013567ffffffffffffffff8111156107635761076261035b565b5b61076f8e828f016105b5565b955095505060c06107828e828f01610633565b93505060e06107938e828f01610668565b9250506101006107a58e828f0161069f565b9150509295989b509295989b9093969950565b5f819050919050565b6107ca816107b8565b81146107d4575f5ffd5b50565b5f813590506107e5816107c1565b92915050565b5f5f5f6060848603121561080257610801610357565b5b5f61080f8682870161037e565b935050602061082086828701610403565b9250506040610831868287016107d7565b9150509250925092565b610844816103dc565b82525050565b5f60208201905061085d5f83018461083b565b92915050565b5f82825260208201905092915050565b828183375f83830152505050565b5f601f19601f8301169050919050565b5f61089c8385610863565b93506108a9838584610873565b6108b283610881565b840190509392505050565b5f6020820190508181035f8301526108d6818486610891565b90509392505050565b6108e88161035f565b82525050565b5f82825260208201905092915050565b5f61090983856108ee565b9350610916838584610873565b61091f83610881565b840190509392505050565b6109338161060a565b82525050565b61094281610647565b82525050565b6109518161067c565b82525050565b5f60c08201905061096a5f83018b6108df565b818103602083015261097d81898b6108fe565b905081810360408301526109928187896108fe565b90506109a1606083018661092a565b6109ae6080830185610939565b6109bb60a0830184610948565b9998505050505050505050565b6109d1816107b8565b82525050565b5f6020820190506109ea5f8301846109c8565b9291505056fea26469706673582212201eac20d11113061d76e398d70d375f6ac3b8a37fe29520760feb7ce338790eea64736f6c63430008210033")]
    contract MockIdentityNames {
        function emitIdentityBound(
            address owner,
            bytes32 idNode,
            bytes32 handleNode,
            bytes32 platformId,
            string calldata userId,
            string calldata handle,
            uint64 observedAt,
            bool published,
            uint16 ceremonyVersion
        ) external;
        function emitCeremonyBound(
            bytes32 authorizationDigest,
            address owner,
            bytes32 platformId,
            bytes calldata clientIdentifier
        ) external;
        function emitBindFeePaid(bytes32 authorizationDigest, address receiver, uint256 amount) external;
        function emitHandleRetired(bytes32 platformId, bytes32 handleNode, address owner) external;
        function emitPlatformConfigured(bytes32 platformId) external;
        function emitProofVerifierConfigured(address verifier) external;
        function emitNameUnpublished(address owner, bytes32 platformId) external;
    }
}

/// Not 31337: the read-model tests use anvil's default id against the same
/// database, and chain_id keying is exactly the isolation this exercises.
const CHAIN: u64 = 43117;

/// A Google account id as the chain binds it: `0x` and the hex SHA-256 of
/// `"libid.google-user-id" || sub`. This is libid-contracts'
/// `GooglePlatformVerifier` test vector for the sub `123456789012345678901`.
const GOOGLE_USER_ID: &str =
    "0x20078023c9d4bf6bffc2580ec36446075d10c8453cecbe4f1cb3d326b2b35560";

#[tokio::test]
async fn indexes_a_real_chain_end_to_end() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if std::process::Command::new("anvil")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: anvil not on PATH");
        return;
    }

    let pool = db::connect_and_migrate(&url).await.expect("database");
    let store = ChainStore::new(pool.clone(), CHAIN as i64);
    // A previous run of this test left rows under this chain id; the loop
    // must start from a clean slate to make block-number assertions exact.
    for table in db::PROJECTION_TABLES {
        sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
            .bind(CHAIN as i64)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
    sqlx::query("DELETE FROM names.chain_metadata WHERE chain_id = $1")
        .bind(CHAIN as i64)
        .execute(&pool)
        .await
        .expect("metadata cleanup");

    let provider = ProviderBuilder::new()
        .connect_anvil_with_wallet_and_config(|anvil| anvil.chain_id(CHAIN))
        .expect("anvil spawns");

    // Blocks before the deploy give the binary search something to find.
    provider.anvil_mine(Some(5), None).await.expect("mine");

    let mock = MockIdentityNames::deploy(provider.clone())
        .await
        .expect("deploy");
    let deploy_block = provider.get_block_number().await.expect("block");

    let x = nodes::Platform::from_key("x").unwrap().id();
    let google = nodes::Platform::from_key("google").unwrap().id();
    let alice = Address::repeat_byte(0xA1);
    let verifier = Address::repeat_byte(0xEE);
    let fee_receiver = Address::repeat_byte(0xFE);
    let digest = B256::repeat_byte(0xD1);

    // The scenario, one tx per step (anvil automines a block each): wire the
    // platform and the Proof Verifier, bind published — the ceremony's own
    // events beside it — rename, add a Google identity, withdraw the display
    // name.
    mock.emitPlatformConfigured(x)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitProofVerifierConfigured(verifier)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitIdentityBound(
        alice,
        nodes::id_node(x, "111"),
        nodes::handle_node(x, &nodes::NormalizedHandle::from_chain("alice_1")),
        x,
        "111".into(),
        "alice_1".into(),
        1000,
        true,
        1,
    )
    .send()
    .await
    .unwrap()
    .watch()
    .await
    .unwrap();
    mock.emitCeremonyBound(digest, alice, x, Bytes::from_static(b"app-a"))
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
    mock.emitHandleRetired(
        x,
        nodes::handle_node(x, &nodes::NormalizedHandle::from_chain("alice_1")),
        alice,
    )
    .send()
    .await
    .unwrap()
    .watch()
    .await
    .unwrap();
    mock.emitIdentityBound(
        alice,
        nodes::id_node(x, "111"),
        nodes::handle_node(x, &nodes::NormalizedHandle::from_chain("alice_2")),
        x,
        "111".into(),
        "alice_2".into(),
        2000,
        true,
        1,
    )
    .send()
    .await
    .unwrap()
    .watch()
    .await
    .unwrap();
    mock.emitIdentityBound(
        alice,
        nodes::id_node(google, GOOGLE_USER_ID),
        nodes::handle_node(
            google,
            &nodes::NormalizedHandle::from_chain("a.b+tag@example.com"),
        ),
        google,
        GOOGLE_USER_ID.into(),
        "a.b+tag@example.com".into(),
        3000,
        false,
        1,
    )
    .send()
    .await
    .unwrap()
    .watch()
    .await
    .unwrap();
    mock.emitNameUnpublished(alice, x)
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
        .prepare(&writer, *mock.address())
        .await
        .expect("prepare");

    // Run the real loop: no start override (detection must find the deploy
    // block), a tiny window so catch-up spans several chunks, no
    // confirmation lag on a chain that cannot reorg.
    let cancel = CancellationToken::new();
    let config = indexer::IndexerConfig {
        contract: *mock.address(),
        confirmations: 0,
        poll_interval_secs: 1,
        max_block_range: 2,
        start_block: None,
        // A report good for two minutes; the loop renews it every cycle.
        stale_after_secs: 120,
    };
    let task = tokio::spawn(
        indexer::Indexer::new(store.clone(), provider.clone(), config)
            .run(cancel.clone()),
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let cursor = store.cursor().await.expect("cursor");
        if cursor.is_some_and(|c| c >= latest) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "indexer did not catch up to block {latest}; cursor {cursor:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    cancel.cancel();
    let _ = task.await;
    writer.release().await.expect("the lease releases");

    // Detection ran once and its answer was cached — and it is the right one.
    let cached = store.deploy_block(*mock.address()).await.expect("metadata");
    assert_eq!(cached, Some(deploy_block), "cached deployment block");

    // alice_1 retired, alice_2 resolves, and the id followed the rename.
    let (status, _) = get::<HandleResolution>(&store, "/v1/resolve/handle/x/alice_1")
        .await
        .refusal();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let resolved: HandleResolution =
        get(&store, "/v1/resolve/handle/x/alice_2").await.answer();
    let binding = &resolved.bindings[0];
    assert_eq!(binding.chain_id, CHAIN as i64);
    assert_eq!(binding.owner, alice);
    assert_eq!(binding.ceremony_version, 1);
    let resolved: IdResolution = get(&store, "/v1/resolve/id/x/111").await.answer();
    assert_eq!(resolved.bindings[0].handle.as_deref(), Some("alice_2"));

    // The Google identity resolves through the URL-encoded raw form.
    let resolved: HandleResolution =
        get(&store, "/v1/resolve/handle/google/A.B%2Btag%40Example.COM")
            .await
            .answer();
    assert_eq!(resolved.handle, "a.b+tag@example.com");
    assert_eq!(
        resolved.bindings[0].user_id.as_deref(),
        Some(GOOGLE_USER_ID)
    );
    let resolved: IdResolution =
        get(&store, &format!("/v1/resolve/id/google/{GOOGLE_USER_ID}"))
            .await
            .answer();
    assert_eq!(resolved.bindings[0].owner, alice);
    assert_eq!(
        resolved.bindings[0].handle.as_deref(),
        Some("a.b+tag@example.com")
    );

    // Reverse: both identities, neither displayed (x was unpublished, google
    // never was).
    let resolved: AddressResolution =
        get(&store, &format!("/v1/resolve/address/{alice}"))
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
        get(&store, "/v1/search?q=alice&platform=x").await.answer();
    let handles: Vec<&str> = results.hits.iter().map(|h| h.handle.as_str()).collect();
    assert_eq!(handles, ["alice_2"], "{results:?}");

    // Ops metadata filled from the admin events, on this chain's entry of
    // the status.
    let status: Status = common::get(&store, "/v1/status").await.answer();
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
    let platform_key: Option<String> = sqlx::query_scalar(
        "SELECT platform_key FROM names.platforms WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(CHAIN as i64)
    .bind(x.as_slice())
    .fetch_one(&pool)
    .await
    .expect("platform row");
    assert_eq!(platform_key.as_deref(), Some("x"));

    // The journal holds every event the scenario emitted, the ceremony's own
    // two included.
    let journal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.events WHERE chain_id = $1")
            .bind(CHAIN as i64)
            .fetch_one(&pool)
            .await
            .expect("journal count");
    assert_eq!(journal, 9);
    let fee_kinds: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM names.events WHERE chain_id = $1 AND kind = 'bind_fee_paid'",
    )
    .bind(CHAIN as i64)
    .fetch_one(&pool)
    .await
    .expect("fee count");
    assert_eq!(fee_kinds, 1);
}
