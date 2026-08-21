//! End to end against a real chain: anvil runs, a mock contract with the
//! exact IdentityNames event surface emits a scenario, and the indexer's own
//! loop — deployment-block detection included — mirrors it into Postgres,
//! where the API answers.
//!
//! Skips silently in exactly two cases: `DATABASE_URL` unset, or no `anvil`
//! binary on PATH. Everything past those checks panics on failure.

use std::time::Duration;

use alloy::{
    primitives::Address,
    providers::{
        ext::AnvilApi,
        Provider,
        ProviderBuilder,
    },
    sol,
};
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
};
use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use usernames_indexer::{
    api,
    db,
    indexer,
    nodes,
};

sol! {
    /// The event surface of IdentityNames behind bare emit functions; source
    /// and regeneration notes in contracts/MockIdentityNames.sol.
    /// (The nine-argument emitter mirrors the nine-field event; clippy's
    /// arity taste does not apply to an ABI.)
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    #[sol(bytecode = "0x6080604052348015600e575f5ffd5b5061089a8061001c5f395ff3fe608060405234801561000f575f5ffd5b506004361061007a575f3560e01c806370b582b91161005957806370b582b9146100d25780638ab9845c146100ee5780638e3c3a391461010a578063e1605549146101265761007a565b806243c8921461007e57806347ad7d271461009a5780636f4f9784146100b6575b5f5ffd5b6100986004803603810190610093919061039a565b610142565b005b6100b460048036038101906100af91906103d8565b61017a565b005b6100d060048036038101906100cb919061039a565b6101aa565b005b6100ec60048036038101906100e7919061045d565b6101e2565b005b610108600480360381019061010391906104ad565b61022c565b005b610124600480360381019061011f9190610528565b610274565b005b610140600480360381019061013b9190610622565b6102bb565b005b8063ffffffff16827f39de5aa871268e261cc41a46876cebce3b7b112a7b158b360f850538860a9bab60405160405180910390a35050565b807fb01c0a30cea93f12bdaa4228f3ced17148be2cc93045c1848d99d588c0141c8f60405160405180910390a250565b8063ffffffff16827f2af3e10bdbb7f8ceef8368a212ed59736df2eb37120a7ef435c48d980c04480a60405160405180910390a35050565b8073ffffffffffffffffffffffffffffffffffffffff1682847f278e2122d24782b703f05416ac53a9750d67bcf6fa1603b94ec95660821a891460405160405180910390a4505050565b808273ffffffffffffffffffffffffffffffffffffffff167f8e43ba98be6948052c3368d53fef5505611df0ae5994d278a646eb9ad3479edd60405160405180910390a35050565b8263ffffffff16847f13b9f8bb5ee05a42b59cc1d4197d78c5ca07a7236ce1bd191161faca7e5bb39884846040516102ad929190610745565b60405180910390a350505050565b888a8c73ffffffffffffffffffffffffffffffffffffffff167f8acbd4495dbb2ff215c4799cd4551da5688633536a4c5381ff06eaca80119a2e8b8b8b8b8b8b8b8b6040516103119897969594939291906107f3565b60405180910390a45050505050505050505050565b5f5ffd5b5f5ffd5b5f819050919050565b6103408161032e565b811461034a575f5ffd5b50565b5f8135905061035b81610337565b92915050565b5f63ffffffff82169050919050565b61037981610361565b8114610383575f5ffd5b50565b5f8135905061039481610370565b92915050565b5f5f604083850312156103b0576103af610326565b5b5f6103bd8582860161034d565b92505060206103ce85828601610386565b9150509250929050565b5f602082840312156103ed576103ec610326565b5b5f6103fa8482850161034d565b91505092915050565b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f61042c82610403565b9050919050565b61043c81610422565b8114610446575f5ffd5b50565b5f8135905061045781610433565b92915050565b5f5f5f6060848603121561047457610473610326565b5b5f6104818682870161034d565b93505060206104928682870161034d565b92505060406104a386828701610449565b9150509250925092565b5f5f604083850312156104c3576104c2610326565b5b5f6104d085828601610449565b92505060206104e18582860161034d565b9150509250929050565b5f67ffffffffffffffff82169050919050565b610507816104eb565b8114610511575f5ffd5b50565b5f81359050610522816104fe565b92915050565b5f5f5f5f608085870312156105405761053f610326565b5b5f61054d8782880161034d565b945050602061055e87828801610386565b935050604061056f87828801610449565b925050606061058087828801610514565b91505092959194509250565b5f5ffd5b5f5ffd5b5f5ffd5b5f5f83601f8401126105ad576105ac61058c565b5b8235905067ffffffffffffffff8111156105ca576105c9610590565b5b6020830191508360018202830111156105e6576105e5610594565b5b9250929050565b5f8115159050919050565b610601816105ed565b811461060b575f5ffd5b50565b5f8135905061061c816105f8565b92915050565b5f5f5f5f5f5f5f5f5f5f5f6101208c8e03121561064257610641610326565b5b5f61064f8e828f01610449565b9b505060206106608e828f0161034d565b9a505060406106718e828f0161034d565b99505060606106828e828f0161034d565b98505060808c013567ffffffffffffffff8111156106a3576106a261032a565b5b6106af8e828f01610598565b975097505060a08c013567ffffffffffffffff8111156106d2576106d161032a565b5b6106de8e828f01610598565b955095505060c06106f18e828f01610514565b93505060e06107028e828f0161060e565b9250506101006107148e828f01610386565b9150509295989b509295989b9093969950565b61073081610422565b82525050565b61073f816104eb565b82525050565b5f6040820190506107585f830185610727565b6107656020830184610736565b9392505050565b6107758161032e565b82525050565b5f82825260208201905092915050565b828183375f83830152505050565b5f601f19601f8301169050919050565b5f6107b4838561077b565b93506107c183858461078b565b6107ca83610799565b840190509392505050565b6107de816105ed565b82525050565b6107ed81610361565b82525050565b5f60c0820190506108065f83018b61076c565b818103602083015261081981898b6107a9565b9050818103604083015261082e8187896107a9565b905061083d6060830186610736565b61084a60808301856107d5565b61085760a08301846107e4565b999850505050505050505056fea2646970667358221220e230f67f255d8f59ba67744ea4079242c2f4c09919cae83d3271174f55db266d64736f6c63430008210033")]
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
            uint32 version
        ) external;
        function emitHandleRetired(bytes32 platformId, bytes32 handleNode, address owner) external;
        function emitPlatformConfigured(bytes32 platformId) external;
        function emitVerifierConfigured(
            bytes32 platformId,
            uint32 version,
            address verifier,
            uint64 maxFutureObservation
        ) external;
        function emitVerifierRetired(bytes32 platformId, uint32 version) external;
        function emitLatestVersionChanged(bytes32 platformId, uint32 version) external;
        function emitNameUnpublished(address owner, bytes32 platformId) external;
    }
}

/// Not 31337: the read-model tests use anvil's default id against the same
/// database, and chain_id keying is exactly the isolation this exercises.
const CHAIN: u64 = 43117;

async fn get(
    pool: &sqlx::PgPool,
    contract: Address,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let state = api::AppState {
        pool: pool.clone(),
        chain_id: CHAIN as i64,
        contract,
    };
    let response = api::router(state)
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}

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
    // A previous run of this test left rows under this chain id; the loop
    // must start from a clean slate to make block-number assertions exact.
    for table in [
        "events",
        "ids",
        "handles",
        "published",
        "platforms",
        "verifiers",
    ] {
        sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
            .bind(CHAIN as i64)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
    sqlx::query("DELETE FROM names.pipeline_metadata WHERE key LIKE '%' || $1 || '%'")
        .bind(CHAIN.to_string())
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

    let x = nodes::platform_id_for_key("x").unwrap();
    let google = nodes::platform_id_for_key("google").unwrap();
    let alice = Address::repeat_byte(0xA1);
    let verifier = Address::repeat_byte(0xEE);

    // The scenario, one tx per step (anvil automines a block each):
    // wire the platform, bind published, rename, add a Google identity,
    // withdraw the display name.
    mock.emitPlatformConfigured(x)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitVerifierConfigured(x, 1, verifier, 300)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitLatestVersionChanged(x, 1)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitIdentityBound(
        alice,
        nodes::id_node(x, "111"),
        nodes::handle_node(x, "alice_1"),
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
    mock.emitHandleRetired(x, nodes::handle_node(x, "alice_1"), alice)
        .send()
        .await
        .unwrap()
        .watch()
        .await
        .unwrap();
    mock.emitIdentityBound(
        alice,
        nodes::id_node(x, "111"),
        nodes::handle_node(x, "alice_2"),
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
        nodes::handle_node(google, "a.b+tag@example.com"),
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
        chain_id: CHAIN as i64,
        confirmations: 0,
        poll_interval_secs: 1,
        max_block_range: 2,
        start_block: None,
    };
    let task = tokio::spawn(indexer::run(
        pool.clone(),
        provider.clone(),
        config,
        cancel.clone(),
    ));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let cursor = db::get_cursor(&pool, CHAIN as i64).await.expect("cursor");
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
    let cached = db::get_deploy_block(&pool, CHAIN as i64, *mock.address())
        .await
        .expect("metadata");
    assert_eq!(cached, Some(deploy_block), "cached deployment block");

    let contract = *mock.address();

    // alice_1 retired, alice_2 resolves, and the id followed the rename.
    let (status, _) = get(&pool, contract, "/v1/resolve/handle/x/alice_1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = get(&pool, contract, "/v1/resolve/handle/x/alice_2").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], alice.to_string().as_str());
    assert_eq!(body["idAgrees"], true);
    let (_, body) = get(&pool, contract, "/v1/resolve/id/x/111").await;
    assert_eq!(body["handle"], "alice_2");

    // The Google identity resolves through the URL-encoded raw form.
    let (status, body) = get(
        &pool,
        contract,
        "/v1/resolve/handle/google/A.B%2Btag%40Example.COM",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["handle"], "a.b+tag@example.com");
    assert_eq!(body["userId"], "999");

    // Reverse: both identities, neither displayed (x was unpublished, google
    // never was).
    let (_, body) = get(&pool, contract, &format!("/v1/resolve/address/{alice}")).await;
    let identities = body["identities"].as_array().unwrap();
    assert_eq!(identities.len(), 2, "{body}");
    assert!(identities.iter().all(|i| i["published"] == false), "{body}");
    assert!(identities.iter().all(|i| i["resolves"] == true), "{body}");

    // Search sees the current handle, not the retired one.
    let (_, body) = get(&pool, contract, "/v1/search?q=alice&platform=x").await;
    let handles: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["handle"].as_str().unwrap())
        .collect();
    assert_eq!(handles, ["alice_2"], "{body}");

    // Ops tables filled from the admin events.
    let (_, body) = get(&pool, contract, "/v1/status").await;
    assert!(
        body["lastIndexedBlock"]
            .as_u64()
            .is_some_and(|b| b >= latest),
        "{body}"
    );
    let latest_version: i64 = sqlx::query_scalar(
        "SELECT latest_version FROM names.platforms WHERE chain_id = $1 AND platform_id = $2",
    )
    .bind(CHAIN as i64)
    .bind(x.as_slice())
    .fetch_one(&pool)
    .await
    .expect("platform row");
    assert_eq!(latest_version, 1);

    // The journal holds every event the scenario emitted.
    let journal: i64 =
        sqlx::query_scalar("SELECT count(*) FROM names.events WHERE chain_id = $1")
            .bind(CHAIN as i64)
            .fetch_one(&pool)
            .await
            .expect("journal count");
    assert_eq!(journal, 8);
}
