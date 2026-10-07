# Contract sources used by the tests

`MockIdentityRegistry.sol` and `MockHandleEscrow.sol` are the indexer's test
doubles: the exact event surfaces of `IdentityRegistry` and `HandleEscrow`,
each event behind a function that emits it, so the anvil suite decodes real
ABI-encoded logs from two emitters. The escrow double also answers `registry`,
which the indexer reads at startup. Each is compiled once and its creation
bytecode pasted into a `sol!` block in `crates/usernames-core/tests/anvil.rs`;
the sources are here so the literals can be regenerated and diffed rather than
trusted.

To regenerate the literals, in a foundry project holding only these files:

```sh
forge build --use 0.8.33
jq -r .bytecode.object out/MockIdentityRegistry.sol/MockIdentityRegistry.json
jq -r .bytecode.object out/MockHandleEscrow.sol/MockHandleEscrow.json
```

The ENS resolver the API's end-to-end test deploys is the `libid-contracts`
crate's embedded `HandleResolver` artifact.
