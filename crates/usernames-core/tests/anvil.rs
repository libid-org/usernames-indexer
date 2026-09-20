//! End to end against a real chain: anvil runs, a mock contract with the
//! exact IdentityNames event surface emits a scenario, and the indexer's own
//! loop — deployment-block detection included — mirrors it into Postgres,
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
use tokio_util::sync::CancellationToken;
use usernames_core::{
    db::{
        self,
        ChainStore,
    },
    indexer,
    nodes,
};

mod common;
use common::get;

sol! {
    /// The event surface of IdentityNames behind bare emit functions; source
    /// and regeneration notes in contracts/MockIdentityNames.sol.
    /// (The nine-argument emitter mirrors the nine-field event; clippy's
    /// arity taste does not apply to an ABI.)
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    #[sol(bytecode = "0x6080604052348015600e575f5ffd5b50610a268061001c5f395ff3fe608060405234801561000f575f5ffd5b506004361061007b575f3560e01c806370b582b91161005957806370b582b9146100d35780638ab9845c146100ef578063e49087d81461010b578063ed3435ae146101275761007b565b806347ad7d271461007f5780635a13374f1461009b57806365186ce6146100b7575b5f5ffd5b61009960048036038101906100949190610392565b610143565b005b6100b560048036038101906100b0919061044a565b610173565b005b6100d160048036038101906100cc919061049a565b6101c7565b005b6100ed60048036038101906100e891906104c5565b610201565b005b61010960048036038101906101049190610515565b61024b565b005b610125600480360381019061012091906105b4565b610293565b005b610141600480360381019061013c9190610736565b6102ec565b005b807fb01c0a30cea93f12bdaa4228f3ced17148be2cc93045c1848d99d588c0141c8f60405160405180910390a250565b8173ffffffffffffffffffffffffffffffffffffffff16837f86eeb882525d52c6bf9923371ee6b2753b7ca825db260239939bfa88f1c530eb836040516101ba919061084a565b60405180910390a3505050565b7f01970f3cbef68f8ac95615ab5c75299e421a83a787173408d693928ed40b6574816040516101f69190610872565b60405180910390a150565b8073ffffffffffffffffffffffffffffffffffffffff1682847f278e2122d24782b703f05416ac53a9750d67bcf6fa1603b94ec95660821a891460405160405180910390a4505050565b808273ffffffffffffffffffffffffffffffffffffffff167f8e43ba98be6948052c3368d53fef5505611df0ae5994d278a646eb9ad3479edd60405160405180910390a35050565b828473ffffffffffffffffffffffffffffffffffffffff16867ff0f0b831e902ded46acfd6caf87649edb445c2e2733992b2e79cd1420719c19c85856040516102dd9291906108e5565b60405180910390a45050505050565b888a8c73ffffffffffffffffffffffffffffffffffffffff167f8ae08d06a548b84d8340ef35ae06cd4c34983cd87b5554e6c818b8180530950c8b8b8b8b8b8b8b8b60405161034298979695949392919061097f565b60405180910390a45050505050505050505050565b5f5ffd5b5f5ffd5b5f819050919050565b6103718161035f565b811461037b575f5ffd5b50565b5f8135905061038c81610368565b92915050565b5f602082840312156103a7576103a6610357565b5b5f6103b48482850161037e565b91505092915050565b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f6103e6826103bd565b9050919050565b6103f6816103dc565b8114610400575f5ffd5b50565b5f81359050610411816103ed565b92915050565b5f819050919050565b61042981610417565b8114610433575f5ffd5b50565b5f8135905061044481610420565b92915050565b5f5f5f6060848603121561046157610460610357565b5b5f61046e8682870161037e565b935050602061047f86828701610403565b925050604061049086828701610436565b9150509250925092565b5f602082840312156104af576104ae610357565b5b5f6104bc84828501610403565b91505092915050565b5f5f5f606084860312156104dc576104db610357565b5b5f6104e98682870161037e565b93505060206104fa8682870161037e565b925050604061050b86828701610403565b9150509250925092565b5f5f6040838503121561052b5761052a610357565b5b5f61053885828601610403565b92505060206105498582860161037e565b9150509250929050565b5f5ffd5b5f5ffd5b5f5ffd5b5f5f83601f84011261057457610573610553565b5b8235905067ffffffffffffffff81111561059157610590610557565b5b6020830191508360018202830111156105ad576105ac61055b565b5b9250929050565b5f5f5f5f5f608086880312156105cd576105cc610357565b5b5f6105da8882890161037e565b95505060206105eb88828901610403565b94505060406105fc8882890161037e565b935050606086013567ffffffffffffffff81111561061d5761061c61035b565b5b6106298882890161055f565b92509250509295509295909350565b5f5f83601f84011261064d5761064c610553565b5b8235905067ffffffffffffffff81111561066a57610669610557565b5b6020830191508360018202830111156106865761068561055b565b5b9250929050565b5f67ffffffffffffffff82169050919050565b6106a98161068d565b81146106b3575f5ffd5b50565b5f813590506106c4816106a0565b92915050565b5f8115159050919050565b6106de816106ca565b81146106e8575f5ffd5b50565b5f813590506106f9816106d5565b92915050565b5f61ffff82169050919050565b610715816106ff565b811461071f575f5ffd5b50565b5f813590506107308161070c565b92915050565b5f5f5f5f5f5f5f5f5f5f5f6101208c8e03121561075657610755610357565b5b5f6107638e828f01610403565b9b505060206107748e828f0161037e565b9a505060406107858e828f0161037e565b99505060606107968e828f0161037e565b98505060808c013567ffffffffffffffff8111156107b7576107b661035b565b5b6107c38e828f01610638565b975097505060a08c013567ffffffffffffffff8111156107e6576107e561035b565b5b6107f28e828f01610638565b955095505060c06108058e828f016106b6565b93505060e06108168e828f016106eb565b9250506101006108288e828f01610722565b9150509295989b509295989b9093969950565b61084481610417565b82525050565b5f60208201905061085d5f83018461083b565b92915050565b61086c816103dc565b82525050565b5f6020820190506108855f830184610863565b92915050565b5f82825260208201905092915050565b828183375f83830152505050565b5f601f19601f8301169050919050565b5f6108c4838561088b565b93506108d183858461089b565b6108da836108a9565b840190509392505050565b5f6020820190508181035f8301526108fe8184866108b9565b90509392505050565b6109108161035f565b82525050565b5f82825260208201905092915050565b5f6109318385610916565b935061093e83858461089b565b610947836108a9565b840190509392505050565b61095b8161068d565b82525050565b61096a816106ca565b82525050565b610979816106ff565b82525050565b5f60c0820190506109925f83018b610907565b81810360208301526109a581898b610926565b905081810360408301526109ba818789610926565b90506109c96060830186610952565b6109d66080830185610961565b6109e360a0830184610970565b999850505050505050505056fea26469706673582212200dc2b44554aa8f1544403f5cffa965b1a23ee340d2e238c6f395b645b50056d464736f6c63430008210033")]
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
        function emitClaimFeePaid(bytes32 authorizationDigest, address receiver, uint256 amount) external;
        function emitHandleRetired(bytes32 platformId, bytes32 handleNode, address owner) external;
        function emitPlatformConfigured(bytes32 platformId) external;
        function emitProofVerifierConfigured(address verifier) external;
        function emitNameUnpublished(address owner, bytes32 platformId) external;
    }
}

/// Not 31337: the read-model tests use anvil's default id against the same
/// database, and chain_id keying is exactly the isolation this exercises.
const CHAIN: u64 = 43117;

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
    // platform and the Proof Verifier, claim published — the ceremony's own
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
    mock.emitClaimFeePaid(digest, fee_receiver, U256::from(5u64))
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
        nodes::id_node(google, "999"),
        nodes::handle_node(
            google,
            &nodes::NormalizedHandle::from_chain("a.b+tag@example.com"),
        ),
        google,
        "999".into(),
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

    // Detection ran once and its answer was cached — and it is the right one.
    let cached = store.deploy_block(*mock.address()).await.expect("metadata");
    assert_eq!(cached, Some(deploy_block), "cached deployment block");

    let contract = *mock.address();

    // alice_1 retired, alice_2 resolves, and the id followed the rename.
    let (status, _) = get(&store, contract, "/v1/resolve/handle/x/alice_1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = get(&store, contract, "/v1/resolve/handle/x/alice_2").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], alice.to_string().as_str());
    assert_eq!(body["idAgrees"], true);
    assert_eq!(body["ceremonyVersion"], 1);
    let (_, body) = get(&store, contract, "/v1/resolve/id/x/111").await;
    assert_eq!(body["handle"], "alice_2");

    // The Google identity resolves through the URL-encoded raw form.
    let (status, body) = get(
        &store,
        contract,
        "/v1/resolve/handle/google/A.B%2Btag%40Example.COM",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["handle"], "a.b+tag@example.com");
    assert_eq!(body["userId"], "999");

    // Reverse: both identities, neither displayed (x was unpublished, google
    // never was).
    let (_, body) = get(&store, contract, &format!("/v1/resolve/address/{alice}")).await;
    let identities = body["identities"].as_array().unwrap();
    assert_eq!(identities.len(), 2, "{body}");
    assert!(identities.iter().all(|i| i["published"] == false), "{body}");
    assert!(identities.iter().all(|i| i["resolves"] == true), "{body}");

    // Search sees the current handle, not the retired one.
    let (_, body) = get(&store, contract, "/v1/search?q=alice&platform=x").await;
    let handles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["handle"].as_str().unwrap())
        .collect();
    assert_eq!(handles, ["alice_2"], "{body}");

    // Ops metadata filled from the admin events.
    let (_, body) = get(&store, contract, "/v1/status").await;
    // The loop reports beside every target it sets, with an expiry in the
    // future, and the API surfaces both: this is what tells a caught-up
    // mirror from one whose indexer stopped.
    assert!(
        body["indexerReportedAt"].as_u64().is_some(),
        "no report after a real loop ran: {body}"
    );
    assert!(
        body["reportValidFor"].as_i64().is_some_and(|s| s > 0),
        "a fresh report must still be valid: {body}"
    );
    assert!(
        body["lastIndexedBlock"]
            .as_u64()
            .is_some_and(|b| b >= latest),
        "{body}"
    );
    assert_eq!(
        body["proofVerifier"],
        verifier.to_string().as_str(),
        "{body}"
    );
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
        "SELECT count(*) FROM names.events WHERE chain_id = $1 AND kind = 'claim_fee_paid'",
    )
    .bind(CHAIN as i64)
    .fetch_one(&pool)
    .await
    .expect("fee count");
    assert_eq!(fee_kinds, 1);
}
