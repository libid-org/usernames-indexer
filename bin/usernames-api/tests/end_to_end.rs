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
        B256,
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
use usernames_core::{
    db::{
        self,
        ChainStore,
    },
    events::{
        LogPosition,
        NamesEvent,
    },
    nodes,
};

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

    /// The real `HandleResolver`; source and regeneration notes in
    /// `contracts/README.md`.
    #[sol(rpc)]
    #[sol(bytecode = "0x6080604052346104b4576113e78038038061001981610529565b92833981016060828203126104b4576100318261054e565b60208301519091906001600160401b0381116104b45783019181601f840112156104b45782519361006961006486610562565b610529565b93602085878152016020819760051b830101918583116104b45760208101915b8383106104b857505050506040810151906001600160401b0382116104b457019180601f840112156104b45782516100c361006482610562565b9360208086848152019260051b8201019283116104b457602001905b82821061049c575050506001600160a01b0316801561048957600180546001600160a01b03199081169091555f8054918216831781556001600160a01b03909116907f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e09080a36002545f60025580610410575b5091905f925b81518410156102f25761016b8483610579565b5193600254680100000000000000008110156102ca5760018101806002558110156102de5760025f5285517f405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace91909101906001600160401b0381116102ca576101d4825461058d565b601f8111610287575b506020601f82116001146102215781906001959697985f92610216575b50505f19600383901b1c191690841b1790555b01929190610158565b015190505f806101fa565b601f19821697835f52815f20985f5b81811061026f575091600196979899918488959410610257575b505050811b01905561020d565b01515f1960f88460031b161c191690555f808061024a565b838301518b556001909a019960209384019301610230565b818111156101dd576102bc90835f5260205f2090601f840160051c90602085106102c2575b601f82910160051c0391016105c5565b5f6101dd565b5f91506102ac565b634e487b7160e01b5f52604160045260245ffd5b634e487b7160e01b5f52603260045260245ffd5b916040519160208301906020845251809152604083019060408160051b85010192915f905b8282106103cd57867f698ec4bf77690368abfaca0b66e51916585a0ca8b23e7925844ea841a4e7a50387870388a15f5b81518110156103be576001906001600160a01b036103658285610579565b51165f52600360205260405f208260ff19825416179055818060a01b0361038c8285610579565b51167f70f1a3dba165402559aaa92407aa69ed152f584fde0bc213bcf1c47acef771b66020604051858152a201610347565b604051610e0690816105e18239f35b9091929360208080600193603f198a82030186528189518051918291828552018484015e5f828201840152601f01601f1916010196019493919091019101610317565b60025f5260205f205f5b828110610428575050610152565b806001918301610438815461058d565b9081610447575b50500161041a565b81601f5f9311851461045d5750555b5f8061043f565b8183526020832061047991601f0160051c8419019086016105c5565b8082528160208120915555610456565b631e4fbdf760e01b5f525f60045260245ffd5b602080916104a98461054e565b8152019101906100df565b5f80fd5b82516001600160401b0381116104b457820187603f820112156104b4576020810151916001600160401b0383116102ca576104fc601f8401601f1916602001610529565b8381526040838501018a106104b4575f602085819660408397018386015e83010152815201920191610089565b6040519190601f01601f191682016001600160401b038111838210176102ca57604052565b51906001600160a01b03821682036104b457565b6001600160401b0381116102ca5760051b60200190565b80518210156102de5760209160051b010190565b90600182811c921680156105bb575b60208310146105a757565b634e487b7160e01b5f52602260045260245ffd5b91607f169161059c565b5f5b8281106105d357505050565b5f828201556001016105c756fe6080806040526004361015610012575f80fd5b5f3560e01c90816301ffc9a71461099b575080631dcfea091461091a57806331cb61051461089d5780636cc895a914610559578063715018a614610510578063736c0d5b146104d3578063796676be1461046457806379ba5097146103e45780638da5cb5b146103bd5780639061b9231461027e578063e30c397814610256578063f2fde38b146101e4578063f4d4d2f8146100d75763f52c8894146100b6575f80fd5b346100d3575f3660031901126100d3576020600254604051908152f35b5f80fd5b346100d3576100e536610bbe565b9290918101916060828403126100d35781356001600160401b0381116100d35783610111918401610a6a565b936020830135926001600160401b038416948585036100d35760408201356001600160401b0381116100d3576101479201610a6a565b934281106101ce5750846101749361016861017d969461016f943691610a25565b9030610c0e565b610ccb565b90929192610d05565b6001600160a01b03165f8181526003602052604090205490919060ff16156101bb576101b790604051918291602083526020830190610b6d565b0390f35b50635292db3160e01b5f5260045260245ffd5b630bfedba560e41b5f526004524260245260445ffd5b346100d35760203660031901126100d3576101fd6109ee565b610205610c9d565b60018060a01b0316806bffffffffffffffffffffffff60a01b600154161760015560018060a01b035f54167f38d16b8cac22d99fc7c124b9cd0de2d3fa1faef420bfe791d8c362d765e227005f80a3005b346100d3575f3660031901126100d3576001546040516001600160a01b039091168152602090f35b346100d35761028c36610bbe565b916002549283156103ae576102e6926102d8916102c6604051978895639061b92360e01b6020880152604060248801526064870191610c7d565b84810360231901604486015291610c7d565b03601f198101845283610a04565b60405190630556f18360e41b82528060a4830130600485015260a060248501525260c4820160c48260051b8401019160025f527f405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace915f905b828210610384578585036003190160448701528580610380896103618982610b6d565b631e9a9a5f60e31b606485015283810360031901608485015290610b6d565b0390fd5b90919293602060016103a0819360c3198a820301865288610aec565b96019201920190929161033e565b6323b0ed8560e21b5f5260045ffd5b346100d3575f3660031901126100d3575f546040516001600160a01b039091168152602090f35b346100d3575f3660031901126100d357600154336001600160a01b03821603610451576001600160a01b03199081166001555f805433928116831782556001600160a01b0316907f8be0079c531659141344cd1fd0a4f28419497f9722a3daafe3b4186f6b6457e09080a3005b63118cdaa760e01b5f523360045260245ffd5b346100d35760203660031901126100d3576004356002548110156100d35761048b90610a88565b6104c0576104a56104ac6101b79260405192838092610aec565b0382610a04565b604051918291602083526020830190610b6d565b634e487b7160e01b5f525f60045260245ffd5b346100d35760203660031901126100d3576001600160a01b036104f46109ee565b165f526003602052602060ff60405f2054166040519015158152f35b346100d3575f3660031901126100d35760405162461bcd60e51b81526020600482015260116024820152701c995b9bdd5b98d948191a5cd8589b1959607a1b6044820152606490fd5b346100d35760203660031901126100d3576004356001600160401b0381116100d357366023820112156100d35780600401356001600160401b0381116100d3578060051b90602482840101903682116100d3576105b4610c9d565b6105c46020604051940184610a04565b82526020820190819360248101925b82841061085d5785856002545f600255806107c9575b50905f915b805183101561074b5760208360051b820101519260025468010000000000000000811015610737578060016106269201600255610a88565b6104c05784516001600160401b038111610737576106448254610ab4565b601f81116106f4575b506020601f821160011461068f57819060019596975f92610684575b50505f19600383901b1c191690841b1790555b0191906105ee565b015190508780610669565b601f19821696835f52815f20975f5b8181106106dc57509160019697989184889594106106c4575b505050811b01905561067c565b01515f1960f88460031b161c191690558780806106b7565b92986020600181928c8601518155019a01930161069e565b8181111561064d5761072990835f5260205f2090601f840160051c906020851061072f575b601f82910160051c039101610cb0565b8661064d565b5f9150610719565b634e487b7160e01b5f52604160045260245ffd5b6040519060208201906020835251809152604082019060408160051b84010193915f905b82821061079e577f698ec4bf77690368abfaca0b66e51916585a0ca8b23e7925844ea841a4e7a50385870386a1005b909192946020806107bb600193603f198982030186528951610b6d565b97019201920190929161076f565b60025f525f5b8181106107dc57506105e9565b806001917f405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace0161080c8154610ab4565b908161081b575b5050016107cf565b81601f5f931185146108315750555b8580610813565b8183526020832061084d91601f0160051c841901908601610cb0565b808252816020812091555561082a565b83356001600160401b0381116100d3578201366043820112156100d35760209161089283923690604460248201359101610a25565b8152019301926105d3565b346100d35760403660031901126100d3576108b66109ee565b602435908115158092036100d35760207f70f1a3dba165402559aaa92407aa69ed152f584fde0bc213bcf1c47acef771b6916108f0610c9d565b60018060a01b031692835f526003825260405f2060ff1981541660ff8316179055604051908152a2005b346100d35760803660031901126100d3576109336109ee565b602435906001600160401b03821682036100d3576044356001600160401b0381116100d357610966903690600401610a6a565b90606435916001600160401b0383116100d35760209361098d610993943690600401610a6a565b92610c0e565b604051908152f35b346100d35760203660031901126100d3576004359063ffffffff60e01b82168092036100d357602091639061b92360e01b81149081156109dd575b5015158152f35b6301ffc9a760e01b149050836109d6565b600435906001600160a01b03821682036100d357565b90601f801991011681019081106001600160401b0382111761073757604052565b9291926001600160401b0382116107375760405191610a4e601f8201601f191660200184610a04565b8294818452818301116100d3578281602093845f960137010152565b9080601f830112156100d357816020610a8593359101610a25565b90565b600254811015610aa05760025f5260205f2001905f90565b634e487b7160e01b5f52603260045260245ffd5b90600182811c92168015610ae2575b6020831014610ace57565b634e487b7160e01b5f52602260045260245ffd5b91607f1691610ac3565b5f9291815491610afb83610ab4565b8083529260018116908115610b505750600114610b1757505050565b5f9081526020812093945091925b838310610b36575060209250010190565b600181602092949394548385870101520191019190610b25565b915050602093945060ff929192191683830152151560051b010190565b805180835260209291819084018484015e5f828201840152601f01601f1916010190565b9181601f840112156100d3578235916001600160401b0383116100d357602083818601950101116100d357565b60406003198201126100d3576004356001600160401b0381116100d35781610be891600401610b91565b92909291602435906001600160401b0382116100d357610c0a91600401610b91565b9091565b92909160208151910120906020815191012090604051926020840194601960f81b86526bffffffffffffffffffffffff199060601b1660228501526001600160401b0360c01b9060c01b166036840152603e830152605e820152605e8152610c77607e82610a04565b51902090565b908060209392818452848401375f828201840152601f01601f1916010190565b5f546001600160a01b0316330361045157565b5f5b828110610cbe57505050565b5f82820155600101610cb2565b8151919060418303610cfb57610cf49250602082015190606060408401519301515f1a90610d79565b9192909190565b50505f9160029190565b6004811015610d655780610d17575050565b60018103610d2e5763f645eedf60e01b5f5260045ffd5b60028103610d49575063fce698f760e01b5f5260045260245ffd5b600314610d535750565b6335e2f38360e21b5f5260045260245ffd5b634e487b7160e01b5f52602160045260245ffd5b91907f7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a08411610dfb579160209360809260ff5f9560405194855216868401526040830152606082015282805260015afa15610df0575f516001600160a01b03811615610de657905f905f90565b505f906001905f90565b6040513d5f823e3d90fd5b5050505f916003919056")]
    contract HandleResolver {
        constructor(address owner_, string[] memory urls_, address[] memory signers_);
        function resolve(bytes calldata name, bytes calldata data) external view returns (bytes memory);
        function resolveWithProof(bytes calldata response, bytes calldata extraData) external view returns (bytes memory);
        function supportsInterface(bytes4 interfaceId) external pure returns (bool);
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
const GATEWAY_URL: &str = "https://gw.handles.link/{sender}/{data}.json";

/// `alice.x.handles.link` in DNS wire format.
fn wire_name(labels: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for label in labels.iter().chain(["handles", "link"].iter()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// Seed one binding into the read model the gateway answers from.
async fn bind(store: &ChainStore, handle: &str, owner: Address) {
    let platform = nodes::Platform::from_key("x").unwrap().id();
    let event = NamesEvent::IdentityBound {
        owner,
        id_node: nodes::id_node(platform, "42"),
        handle_node: nodes::handle_node(
            platform,
            &nodes::NormalizedHandle::from_chain(handle),
        ),
        platform_id: platform,
        user_id: "42".into(),
        handle: handle.into(),
        observed_at: 1_700_000_000,
        version: 1,
        published: true,
    };
    let mut window = store.begin_window().await.expect("begin");
    window
        .apply(
            &event,
            &LogPosition {
                block_number: 1,
                log_index: 0,
                tx_hash: B256::from([7u8; 32]),
            },
        )
        .await
        .expect("apply");
    window.commit(1).await.expect("commit");
    // The indexer records the head every cycle, and the gateway refuses to
    // answer from a mirror that cannot say how far behind it is. A test that
    // skipped this was relying on "unknown" being read as "fresh" — which is
    // exactly the fail-open the lag gate now closes.
    store.set_chain_head(1).await;
}

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
    let name = Bytes::from(wire_name(&["alice", "x"]));
    let inner = Bytes::from(
        addrCall {
            node: B256::from([1u8; 32]),
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
