# Contracts the tests deploy

The anvil suite deploys the `libid-contracts` crate's embedded artifacts:
`IdentityRegistry`, `CeremonyProofVerifier` and `HandleEscrow`, each behind an
`ERC1967Proxy`. The API's end-to-end test deploys the crate's `HandleResolver`.

`StubPlatformVerifier.json` is libid-contracts' test stand-in for a Platform
Verifier: it answers any payload with the account and handle it was last set
to, so a bind needs no proof. Its ABI and creation bytecode are compiled from
`solidity/contracts/identity/test/StubPlatformVerifier.sol` at the
libid-contracts tag the crate pins, v0.15.0:

```sh
git checkout v0.15.0 && git submodule update --init && cd solidity
forge build contracts/identity/test/StubPlatformVerifier.sol
jq '{abi, bytecode: {object: .bytecode.object}}' \
  out/StubPlatformVerifier.sol/StubPlatformVerifier.json > StubPlatformVerifier.json
```
