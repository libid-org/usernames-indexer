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
        U256,
    },
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{
        SolCall,
        SolError,
    },
};
use axum::{
    body::Body,
    http::Request,
    Router,
};
use http_body_util::BodyExt;
use tokio::sync::Mutex;
use tower::ServiceExt;
use usernames_api::ens::{
    ChainGateway,
    Config,
    GatewayState,
    HandleSource,
};
use usernames_core::db::{
    self,
    ChainStore,
};

mod common;
use common::*;

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

    /// The real `HandleResolver`; source and regeneration notes in
    /// `contracts/README.md`.
    #[sol(rpc)]
    #[sol(bytecode = "0x6080604052346104b45761144a8038038061001981610529565b92833981016060828203126104b4576100318261054e565b60208301519091906001600160401b0381116104b45783019181601f840112156104b45782519361006961006486610562565b610529565b93602085878152016020819760051b830101918583116104b45760208101915b8383106104b857505050506040810151906001600160401b0382116104b457019180601f840112156104b45782516100c361006482610562565b9360208086848152019260051b8201019283116104b457602001905b82821061049c575050506001600160a01b0316801561048957600180546001600160a01b03199081169091555f8054918216831781556001600160a01b03909116907f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e09080a36002545f60025580610410575b5091905f925b81518410156102f25761016b8483610579565b5193600254680100000000000000008110156102ca5760018101806002558110156102de5760025f5285517f405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace91909101906001600160401b0381116102ca576101d4825461058d565b601f8111610287575b506020601f82116001146102215781906001959697985f92610216575b50505f19600383901b1c191690841b1790555b01929190610158565b015190505f806101fa565b601f19821697835f52815f20985f5b81811061026f575091600196979899918488959410610257575b505050811b01905561020d565b01515f1960f88460031b161c191690555f808061024a565b838301518b556001909a019960209384019301610230565b818111156101dd576102bc90835f5260205f2090601f840160051c90602085106102c2575b601f82910160051c0391016105c5565b5f6101dd565b5f91506102ac565b634e487b7160e01b5f52604160045260245ffd5b634e487b7160e01b5f52603260045260245ffd5b916040519160208301906020845251809152604083019060408160051b85010192915f905b8282106103cd57867f698ec4bf77690368abfaca0b66e51916585a0ca8b23e7925844ea841a4e7a50387870388a15f5b81518110156103be576001906001600160a01b036103658285610579565b51165f52600360205260405f208260ff19825416179055818060a01b0361038c8285610579565b51167f70f1a3dba165402559aaa92407aa69ed152f584fde0bc213bcf1c47acef771b66020604051858152a201610347565b604051610e6990816105e18239f35b9091929360208080600193603f198a82030186528189518051918291828552018484015e5f828201840152601f01601f1916010196019493919091019101610317565b60025f5260205f205f5b828110610428575050610152565b806001918301610438815461058d565b9081610447575b50500161041a565b81601f5f9311851461045d5750555b5f8061043f565b8183526020832061047991601f0160051c8419019086016105c5565b8082528160208120915555610456565b631e4fbdf760e01b5f525f60045260245ffd5b602080916104a98461054e565b8152019101906100df565b5f80fd5b82516001600160401b0381116104b457820187603f820112156104b4576020810151916001600160401b0383116102ca576104fc601f8401601f1916602001610529565b8381526040838501018a106104b4575f602085819660408397018386015e83010152815201920191610089565b6040519190601f01601f191682016001600160401b038111838210176102ca57604052565b51906001600160a01b03821682036104b457565b6001600160401b0381116102ca5760051b60200190565b80518210156102de5760209160051b010190565b90600182811c921680156105bb575b60208310146105a757565b634e487b7160e01b5f52602260045260245ffd5b91607f169161059c565b5f5b8281106105d357505050565b5f828201556001016105c756fe6080806040526004361015610012575f80fd5b5f3560e01c90816301ffc9a7146109fe575080631dcfea091461097d5780632ba2a6651461096157806331cb6105146108e45780636cc895a9146105a0578063715018a614610557578063736c0d5b1461051a578063796676be146104ab57806379ba50971461042b5780638da5cb5b146104045780639061b923146102c5578063e30c39781461029d578063f2fde38b1461022b578063f4d4d2f8146100e25763f52c8894146100c1575f80fd5b346100de575f3660031901126100de576020600254604051908152f35b5f80fd5b346100de576100f036610c21565b9290918101916060828403126100de5781356001600160401b0381116100de578361011c918401610acd565b936020830135926001600160401b038416948585036100de5760408201356001600160401b0381116100de576101529201610acd565b9342811061021557610e10420190814211610201578181116101ec578661019b6101928861018d848a610186368b8d610a88565b9030610c71565b610d2e565b90929192610d68565b6001600160a01b03165f8181526003602052604090205490919060ff16156101d9576101d590604051918291602083526020830190610bd0565b0390f35b50635292db3160e01b5f5260045260245ffd5b6309b2bcf360e11b5f5260045260245260445ffd5b634e487b7160e01b5f52601160045260245ffd5b630bfedba560e41b5f526004524260245260445ffd5b346100de5760203660031901126100de57610244610a51565b61024c610d00565b60018060a01b0316806bffffffffffffffffffffffff60a01b600154161760015560018060a01b035f54167f38d16b8cac22d99fc7c124b9cd0de2d3fa1faef420bfe791d8c362d765e227005f80a3005b346100de575f3660031901126100de576001546040516001600160a01b039091168152602090f35b346100de576102d336610c21565b916002549283156103f55761032d9261031f9161030d604051978895639061b92360e01b6020880152604060248801526064870191610ce0565b84810360231901604486015291610ce0565b03601f198101845283610a67565b60405190630556f18360e41b82528060a4830130600485015260a060248501525260c4820160c48260051b8401019160025f527f405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace915f905b8282106103cb5785850360031901604487015285806103c7896103a88982610bd0565b631e9a9a5f60e31b606485015283810360031901608485015290610bd0565b0390fd5b90919293602060016103e7819360c3198a820301865288610b4f565b960192019201909291610385565b6323b0ed8560e21b5f5260045ffd5b346100de575f3660031901126100de575f546040516001600160a01b039091168152602090f35b346100de575f3660031901126100de57600154336001600160a01b03821603610498576001600160a01b03199081166001555f805433928116831782556001600160a01b0316907f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e09080a3005b63118cdaa760e01b5f523360045260245ffd5b346100de5760203660031901126100de576004356002548110156100de576104d290610aeb565b610507576104ec6104f36101d59260405192838092610b4f565b0382610a67565b604051918291602083526020830190610bd0565b634e487b7160e01b5f525f60045260245ffd5b346100de5760203660031901126100de576001600160a01b0361053b610a51565b165f526003602052602060ff60405f2054166040519015158152f35b346100de575f3660031901126100de5760405162461bcd60e51b81526020600482015260116024820152701c995b9bdd5b98d948191a5cd8589b1959607a1b6044820152606490fd5b346100de5760203660031901126100de576004356001600160401b0381116100de57366023820112156100de5780600401356001600160401b0381116100de578060051b90602482840101903682116100de576105fb610d00565b61060b6020604051940184610a67565b82526020820190819360248101925b8284106108a45785856002545f60025580610810575b50905f915b80518310156107925760208360051b82010151926002546801000000000000000081101561077e5780600161066d9201600255610aeb565b6105075784516001600160401b03811161077e5761068b8254610b17565b601f811161073b575b506020601f82116001146106d657819060019596975f926106cb575b50505f19600383901b1c191690841b1790555b019190610635565b0151905087806106b0565b601f19821696835f52815f20975f5b818110610723575091600196979891848895941061070b575b505050811b0190556106c3565b01515f1960f88460031b161c191690558780806106fe565b92986020600181928c8601518155019a0193016106e5565b818111156106945761077090835f5260205f2090601f840160051c9060208510610776575b601f82910160051c039101610d13565b86610694565b5f9150610760565b634e487b7160e01b5f52604160045260245ffd5b6040519060208201906020835251809152604082019060408160051b84010193915f905b8282106107e5577f698ec4bf77690368abfaca0b66e51916585a0ca8b23e7925844ea841a4e7a50385870386a1005b90919294602080610802600193603f198982030186528951610bd0565b9701920192019092916107b6565b60025f525f5b8181106108235750610630565b806001917f405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace016108538154610b17565b9081610862575b505001610816565b81601f5f931185146108785750555b858061085a565b8183526020832061089491601f0160051c841901908601610d13565b8082528160208120915555610871565b83356001600160401b0381116100de578201366043820112156100de576020916108d983923690604460248201359101610a88565b81520193019261061a565b346100de5760403660031901126100de576108fd610a51565b602435908115158092036100de5760207f70f1a3dba165402559aaa92407aa69ed152f584fde0bc213bcf1c47acef771b691610937610d00565b60018060a01b031692835f526003825260405f2060ff1981541660ff8316179055604051908152a2005b346100de575f3660031901126100de576020604051610e108152f35b346100de5760803660031901126100de57610996610a51565b602435906001600160401b03821682036100de576044356001600160401b0381116100de576109c9903690600401610acd565b90606435916001600160401b0383116100de576020936109f06109f6943690600401610acd565b92610c71565b604051908152f35b346100de5760203660031901126100de576004359063ffffffff60e01b82168092036100de57602091639061b92360e01b8114908115610a40575b5015158152f35b6301ffc9a760e01b14905083610a39565b600435906001600160a01b03821682036100de57565b90601f801991011681019081106001600160401b0382111761077e57604052565b9291926001600160401b03821161077e5760405191610ab1601f8201601f191660200184610a67565b8294818452818301116100de578281602093845f960137010152565b9080601f830112156100de57816020610ae893359101610a88565b90565b600254811015610b035760025f5260205f2001905f90565b634e487b7160e01b5f52603260045260245ffd5b90600182811c92168015610b45575b6020831014610b3157565b634e487b7160e01b5f52602260045260245ffd5b91607f1691610b26565b5f9291815491610b5e83610b17565b8083529260018116908115610bb35750600114610b7a57505050565b5f9081526020812093945091925b838310610b99575060209250010190565b600181602092949394548385870101520191019190610b88565b915050602093945060ff929192191683830152151560051b010190565b805180835260209291819084018484015e5f828201840152601f01601f1916010190565b9181601f840112156100de578235916001600160401b0383116100de57602083818601950101116100de57565b60406003198201126100de576004356001600160401b0381116100de5781610c4b91600401610bf4565b92909291602435906001600160401b0382116100de57610c6d91600401610bf4565b9091565b92909160208151910120906020815191012090604051926020840194601960f81b86526bffffffffffffffffffffffff199060601b1660228501526001600160401b0360c01b9060c01b166036840152603e830152605e820152605e8152610cda607e82610a67565b51902090565b908060209392818452848401375f828201840152601f01601f1916010190565b5f546001600160a01b0316330361049857565b5f5b828110610d2157505050565b5f82820155600101610d15565b8151919060418303610d5e57610d579250602082015190606060408401519301515f1a90610ddc565b9192909190565b50505f9160029190565b6004811015610dc85780610d7a575050565b60018103610d915763f645eedf60e01b5f5260045ffd5b60028103610dac575063fce698f760e01b5f5260045260245ffd5b600314610db65750565b6335e2f38360e21b5f5260045260245ffd5b634e487b7160e01b5f52602160045260245ffd5b91907f7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a08411610e5e579160209360809260ff5f9560405194855216868401526040830152606082015282805260015afa15610e53575f516001600160a01b03811615610e4957905f905f90565b505f906001905f90565b6040513d5f823e3d90fd5b5050505f916003919056")]
    contract HandleResolver {
        constructor(address owner_, string[] memory urls_, address[] memory signers_);
        function resolve(bytes calldata name, bytes calldata data) external view returns (bytes memory);
        function resolveWithProof(bytes calldata response, bytes calldata extraData) external view returns (bytes memory);
        function supportsInterface(bytes4 interfaceId) external pure returns (bool);
        function MAX_LIFETIME() external view returns (uint256);
    }

    /// ENSIP-11, as the client asks for it.
    function addr(bytes32 node, uint256 coinType) external view returns (bytes memory);
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
    let resolver = HandleResolver::deploy(
        provider.clone(),
        deployer.address(),
        vec![GATEWAY_URL.to_string()],
        vec![trusted.address()],
    )
    .await
    .expect("deploy");
    let signer = match who {
        WhoSigns::TheTrustedKey => trusted,
        WhoSigns::AnImpostor => IMPOSTOR_KEY.parse().expect("key"),
    };

    // The gateway is bound to THIS resolver, because the signature names it.
    let router = usernames_api::ens::router(GatewayState::new(Config {
        resolver: *resolver.address(),
        chains: [(
            CHAIN as u64,
            ChainGateway {
                label: None,
                source: HandleSource::Mirror(store),
            },
        )]
        .into_iter()
        .collect(),
        ttl_secs: 300,
        max_lag_blocks: 32,
        signer: std::sync::Arc::new(signer),
    }));

    // ── 1. the wallet asks the resolver, and is told where to look ───
    let labels = ["alice", "x", "handles", "link"].map(String::from);
    let name = Bytes::from(wire_name(&["alice", "x"]));
    let inner = Bytes::from(
        addrCall {
            // The namehash of the same name, as a wallet sends it: the gateway
            // checks that the request's two halves describe one name.
            node: usernames_core::ens::namehash(&labels),
            coinType: U256::from(0x8000_0000u64 | CHAIN as u64),
        }
        .abi_encode(),
    );
    let err = resolver
        .resolve(name.clone(), inner.clone())
        .call()
        .await
        .expect_err("resolve must revert with OffchainLookup, that IS the protocol");

    // ── 2. the endpoint and the query travel inside the revert ───────
    let raw = err.as_revert_data().expect("the revert carries data");
    let lookup = OffchainLookup::abi_decode(&raw).expect("OffchainLookup");
    assert_eq!(lookup.sender, *resolver.address());
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
