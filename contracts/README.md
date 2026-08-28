# Contract sources used by the tests

These are copies, not the source of truth. Each is compiled once and its
creation bytecode pasted into a `sol!` block in the test that deploys it; the
copy is here so the pasted bytes can be regenerated and diffed rather than
trusted.

| File | Origin | Used by |
|---|---|---|
| `MockIdentityNames.sol` | written here | `crates/usernames-core/tests/anvil.rs` |
| `HandleResolver.sol`, `IExtendedResolver.sol` | [libid-contracts](https://github.com/libid-org/libid-contracts) `solidity/contracts/ens/` | `bin/usernames-api/tests/end_to_end.rs` |

To regenerate a bytecode literal:

```sh
forge build --root . contracts/HandleResolver.sol
jq -r .bytecode.object out/HandleResolver.sol/HandleResolver.json
```

A drifted copy is a test that passes against a resolver nobody deploys, so
when `libid-contracts` changes `HandleResolver`, refresh both the source here
and the literal in the test.
