//! The signature the gateway produces must be the one the resolver accepts.
//!
//! `HandleResolver.resolveWithProof` hands the 65 bytes straight to OpenZeppelin's
//! `ECDSA.recover`, which requires `v` to be 27 or 28 and rejects 0 or 1
//! outright. Signing libraries disagree about which they emit, and the failure
//! mode is total and silent from here: every answer this gateway signs would
//! be refused on chain, and nothing in this process would notice.

use alloy::{
    primitives::{
        keccak256,
        Address,
        B256,
    },
    signers::{
        local::PrivateKeySigner,
        SignerSync,
    },
};
use usernames_core::ens;

fn signer() -> PrivateKeySigner {
    "0x00000000000000000000000000000000000000000000000000000000000a11ce"
        .parse()
        .expect("a valid key")
}

/// The same construction `Reply::digest` builds, spelled out again from the
/// specification rather than reused, so this fails if that method drifts.
fn digest(target: Address, expires: u64, request: &[u8], result: &[u8]) -> B256 {
    let mut preimage = vec![0x19, 0x00];
    preimage.extend_from_slice(target.as_slice());
    preimage.extend_from_slice(&expires.to_be_bytes());
    preimage.extend_from_slice(keccak256(request).as_slice());
    preimage.extend_from_slice(keccak256(result).as_slice());
    keccak256(preimage)
}

#[test]
fn the_recovery_byte_is_the_one_solidity_accepts() {
    let signer = signer();
    // Sign a spread of digests: `v` depends on the signature, so one sample
    // proves nothing about the next.
    for i in 0u8..32 {
        let sig = signer
            .sign_hash_sync(&B256::from([i; 32]))
            .expect("signing a digest");
        let bytes = sig.as_bytes();
        assert_eq!(bytes.len(), 65, "signature must be r || s || v");
        assert!(
            bytes[64] == 27 || bytes[64] == 28,
            "v was {}; OpenZeppelin's ECDSA.recover rejects anything but 27 or 28",
            bytes[64]
        );
    }
}

#[test]
fn the_signer_recovers_from_what_the_gateway_signs() {
    let signer = signer();
    let target = Address::from([0xaa; 20]);
    let request = b"a resolve(bytes,bytes) call".to_vec();
    let result = b"an ABI-encoded address".to_vec();
    let expires = 1_800_000_000u64;

    let expected = digest(target, expires, &request, &result);
    assert_eq!(
        ens::Reply {
            result: result.clone(),
            expires
        }
        .digest(target, &request),
        expected,
        "the module's digest drifted from the specification"
    );

    let sig = signer.sign_hash_sync(&expected).expect("signing");
    assert_eq!(
        sig.recover_address_from_prehash(&expected)
            .expect("recover"),
        signer.address(),
        "the resolver would recover a different signer than the one configured"
    );
}
