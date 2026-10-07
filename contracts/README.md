# Contract sources used by the tests

`MockIdentityRegistry.sol` and `MockHandleEscrow.sol` are the indexer's test
doubles: the exact event surfaces of `IdentityRegistry` and `HandleEscrow`,
each event behind a function that emits it, so the anvil suite decodes real
ABI-encoded logs from two emitters. The escrow double also answers `registry`,
which the indexer reads at startup. Each is compiled once, and the ABI and
creation bytecode of each are committed beside its source as the JSON the
anvil suite's `sol!` reads.

To regenerate them, in a foundry project holding only these sources:

```sh
forge build --use 0.8.33
jq '{abi, bytecode: {object: .bytecode.object}}' \
  out/MockIdentityRegistry.sol/MockIdentityRegistry.json > MockIdentityRegistry.json
jq '{abi, bytecode: {object: .bytecode.object}}' \
  out/MockHandleEscrow.sol/MockHandleEscrow.json > MockHandleEscrow.json
```

The ENS resolver the API's end-to-end test deploys is the `libid-contracts`
crate's embedded `HandleResolver` artifact.
