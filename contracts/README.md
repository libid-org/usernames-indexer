# Contract sources used by the tests

`MockIdentityNames.sol` is the indexer's test double: the exact event surface
of `IdentityNames`, each event behind a function that emits it, so the anvil
suite decodes real ABI-encoded logs. It is compiled once and its creation
bytecode pasted into the `sol!` block in `crates/usernames-core/tests/anvil.rs`;
the source is here so the literal can be regenerated and diffed rather than
trusted.

To regenerate the literal, in a foundry project holding only this file:

```sh
forge build --use 0.8.33
jq -r .bytecode.object out/MockIdentityNames.sol/MockIdentityNames.json
```

The ENS resolver the API's end-to-end test deploys is the `libid-contracts`
crate's embedded `HandleResolver` artifact; nothing of it lives here.
