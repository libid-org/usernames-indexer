//! The whole CCIP-Read loop, against a real resolver on a real chain.
//!
//! Everything else in this suite tests one half. The unit tests prove the
//! parse and the encoding; `gateway.rs` proves the decisions through the
//! router; `signature.rs` proves the recovery byte. None of them puts the two
//! halves together, and the two halves are where this protocol actually
//! lives: a blob this gateway signs is only worth anything if
//! `HandleResolver.resolveWithProof` accepts it.
//!
//! So this deploys the real contract on anvil and walks the protocol exactly
//! as a wallet does:
//!
//!   1. `eth_call resolve(name, data)` -> revert `OffchainLookup`
//!   2. decode the revert; the endpoint and the query travel inside it
//!   3. ask the gateway (in process, through the same router it serves)
//!   4. `eth_call resolveWithProof(response, extraData)` -> the address
//!
//! It would have caught, on its own, every mismatch this gateway hit while it
//! was being written: a recovery byte the contract rejects, a digest
//! preimage that drifted, and a chain the gateway answered for when it should
//! have stepped aside.
//!
//! Skips silently without `DATABASE_URL`, and needs `anvil` on PATH.

use alloy::{
    node_bindings::Anvil,
    primitives::{
        Address,
        Bytes,
    },
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{
        SolError,
        SolValue,
    },
};
use axum::{
    body::Body,
    http::Request,
    Router,
};
use http_body_util::BodyExt;
use libid_contracts::{
    bindings::ens::HandleResolver,
    deploy::deploy_with_ctor,
    Artifacts,
};
use libid_signer::ManagedSigner;
use tokio::sync::Mutex;
use tower::ServiceExt;
use usernames_api::ens::{
    Config,
    GatewayState,
};
use usernames_core::db::{
    self,
    ChainStore,
};

use usernames_fixtures::*;

sol! {
    /// ERC-3668. The resolver reverts with this rather than returning, which
    /// is the protocol and not a failure.
    error OffchainLookup(
        address sender,
        string[] urls,
        bytes callData,
        bytes4 callbackFunction,
        bytes extraData
    );

    /// Nobody in the resolver's signer set signed the answer.
    error UntrustedSigner(address recovered);

    /// The gateway signed a deadline further out than the resolver accepts.
    error DeadlineTooFar(uint64 expires, uint256 ceiling);

    /// The one call the crate's binding leaves out, because on chain it always
    /// reverts `OffchainLookup`: the client half of the protocol, which is
    /// what this test plays.
    #[sol(rpc)]
    contract Resolve {
        function resolve(bytes calldata name, bytes calldata data) external view returns (bytes memory);
    }
}

const CHAIN: i64 = 31343;

/// Both tests here wipe and reseed the same chain's rows, so they must not
/// overlap — libtest runs them on parallel threads in one process, and
/// whichever wipes second deletes the other's binding. The other DB-touching
/// suites hold the same kind of guard.
static DB_LOCK: Mutex<()> = Mutex::const_new(());
const SIGNER_KEY: &str =
    "0x00000000000000000000000000000000000000000000000000000000000a11ce";
const IMPOSTOR_KEY: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000bad";
/// Carries the `/ens` prefix the route is mounted under, so the fixture is the
/// shape an operator would actually put in the resolver's `urls`. The test
/// calls the router directly, so a wrong path here would never fail — which is
/// exactly why it has to be right.
const GATEWAY_URL: &str = "https://gw.handles.link/ens/{sender}/{data}.json";

/// Step 3, in process: hand the gateway what the revert carried, exactly the
/// way an HTTP client would after substituting the placeholders.
async fn ask_gateway(router: &Router, sender: Address, call_data: &Bytes) -> Bytes {
    let uri = format!("/{sender}/0x{}.json", hex::encode(call_data));
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router");
    assert!(
        response.status().is_success(),
        "gateway refused: {}",
        response.status()
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let data = value["data"].as_str().expect("a data field");
    hex::decode(data.trim_start_matches("0x"))
        .expect("hex")
        .into()
}

#[tokio::test]
async fn a_wallet_resolves_a_name_through_the_real_resolver() {
    match walk(WhoSigns::TheTrustedKey).await {
        Some(Ok(address)) => assert_eq!(
            address,
            Address::from([0xbe; 20]),
            "the address the chain returned is not the one the read model holds"
        ),
        Some(Err(e)) => panic!("the resolver refused what the gateway signed: {e}"),
        None => {}
    }
}

/// The same walk, with a gateway key the resolver does not trust.
///
/// Without this, the test above proves only that something returned an
/// address — not that the chain checked anything. `UntrustedSigner` is the
/// resolver's own error, so this also pins that the failure arrives from the
/// contract rather than from anywhere in this process.
#[tokio::test]
async fn the_chain_refuses_a_blob_signed_by_a_key_it_does_not_know() {
    match walk(WhoSigns::AnImpostor).await {
        Some(Ok(address)) => panic!("an untrusted signer resolved to {address}"),
        Some(Err(e)) => {
            // Decoded, not matched on prose: the selector is the contract's
            // own, so this asserts the refusal came from the resolver rather
            // than from anywhere in this process.
            let raw = e.as_revert_data().expect("the revert carries data");
            let refused = UntrustedSigner::abi_decode(&raw)
                .expect("expected the resolver's UntrustedSigner");
            assert_eq!(
                refused.recovered,
                IMPOSTOR_KEY.parse::<PrivateKeySigner>().unwrap().address(),
                "the resolver recovered a signer other than the one that signed"
            );
        }
        None => {}
    }
}

enum WhoSigns {
    TheTrustedKey,
    AnImpostor,
}

/// The four protocol steps, end to end. `None` means the suite skipped.
async fn walk(who: WhoSigns) -> Option<Result<Address, alloy::contract::Error>> {
    let _guard = DB_LOCK.lock().await;
    let url = std::env::var("DATABASE_URL").ok()?;

    // ── the read model the gateway answers from ──────────────────────
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    db::MIGRATOR.run(&pool).await.expect("migrations");
    for table in db::PROJECTION_TABLES {
        sqlx::query(&format!("DELETE FROM names.{table} WHERE chain_id = $1"))
            .bind(CHAIN)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
    sqlx::query("DELETE FROM names.chain_metadata WHERE chain_id = $1")
        .bind(CHAIN)
        .execute(&pool)
        .await
        .expect("cleanup");
    let store = ChainStore::new(pool, CHAIN);
    let owner = Address::from([0xbe; 20]);
    bind(&store, "alice", owner).await;

    // ── the chain, and the resolver on it ────────────────────────────
    let anvil = Anvil::new().try_spawn().expect("anvil on PATH");
    let deployer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let provider = ProviderBuilder::new()
        .wallet(deployer.clone())
        .connect_http(anvil.endpoint_url());

    // The resolver trusts exactly one key; the gateway may or may not hold it.
    let trusted: PrivateKeySigner = SIGNER_KEY.parse().expect("key");
    // The resolver the release ships, from the crate's embedded artifact.
    let constructor = (
        deployer.address(),
        vec![GATEWAY_URL.to_string()],
        vec![trusted.address()],
    )
        .abi_encode_params();
    let resolver_address = deploy_with_ctor(
        &provider,
        &Artifacts::embedded()
            .bytecode("HandleResolver")
            .expect("the crate ships the resolver"),
        &constructor,
        "HandleResolver",
        None,
    )
    .await
    .expect("deploy");
    let resolver = HandleResolver::new(resolver_address, provider.clone());
    let client = Resolve::new(resolver_address, provider.clone());
    let signer = match who {
        WhoSigns::TheTrustedKey => trusted,
        WhoSigns::AnImpostor => IMPOSTOR_KEY.parse().expect("key"),
    };

    // The gateway is bound to THIS resolver, because the signature names it.
    let router = usernames_api::ens::router(GatewayState::new(Config {
        resolver: resolver_address,
        store: db::Store::new(store.pool().clone()),
        ttl_secs: 300,
        max_lag_blocks: 32,
        signer: std::sync::Arc::new(ManagedSigner::Local(signer)),
    }));

    // ── 1. the wallet asks the resolver, and is told where to look ───
    let name = Bytes::from(wire_name(&["alice", "x"]));
    // The namehash of the same name rides inside, as a wallet sends it: the
    // gateway checks that the request's two halves describe one name.
    let inner = Bytes::from(addr_call(&["alice", "x"], 0x8000_0000u64 | CHAIN as u64));
    let err = client
        .resolve(name.clone(), inner.clone())
        .call()
        .await
        .expect_err("resolve must revert with OffchainLookup, that IS the protocol");

    // ── 2. the endpoint and the query travel inside the revert ───────
    let raw = err.as_revert_data().expect("the revert carries data");
    let lookup = OffchainLookup::abi_decode(&raw).expect("OffchainLookup");
    assert_eq!(lookup.sender, resolver_address);
    assert_eq!(lookup.urls, vec![GATEWAY_URL.to_string()]);

    // ── 3. the client fetches a signed blob ──────────────────────────
    let response = ask_gateway(&router, lookup.sender, &lookup.callData).await;

    // ── 4. and the chain turns it into an address, or refuses ────────
    Some(
        resolver
            .resolveWithProof(response, lookup.extraData)
            .call()
            .await
            .map(|returned| {
                // ENSIP-11 `addr` returns `bytes`; a wallet reads an address
                // out of them.
                let decoded =
                    <Bytes as alloy::sol_types::SolValue>::abi_decode(&returned)
                        .expect("the record decodes as bytes");
                Address::from_slice(&decoded)
            }),
    )
}
