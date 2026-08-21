# usernames-indexer

Indexes [`IdentityNames`](https://github.com/libid-org/libid-contracts/blob/main/solidity/contracts/identity/IdentityNames.sol)
events into Postgres and serves resolution and search over the claimed
handles. One binary, two halves: a polling loop that mirrors the contract's
storage from its events alone, and a read API.

The contract was designed for exactly this: `IdentityBound` carries the
plaintext `userId` and the normalized `handle` next to their storage nodes,
so the read model needs no on-chain strings, and the `published` flag plus
`NameUnpublished` are emitted precisely so an off-chain mirror can reproduce
reverse display without guessing.

## Running

```sh
docker compose up -d postgres          # listens on 127.0.0.1:55432
cp .env.example .env                   # fill in RPC_URL + IDENTITY_NAMES_ADDRESS
cargo run -p usernames-indexer
```

The compose database's DSN is
`postgres://usernames:usernames_dev@127.0.0.1:55432/usernames` (already in
`.env.example`). The migrations run `CREATE EXTENSION pg_trgm`, which needs a
role allowed to create extensions — true for the compose database; on a
managed instance with a least-privilege role, have an administrator run it
once beforehand.

Configuration is flags or environment (a `.env` file is read first):

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | — | Postgres connection string |
| `RPC_URL` | — | JSON-RPC endpoint of the chain to follow. Prefer a single node or a sticky endpoint: a load balancer that mixes lagged replicas can answer `eth_getLogs` for blocks a backend has not seen, and events dropped that way past the confirmation margin are gone until a re-index. The loop re-checks the backend's height before committing a window, which narrows but cannot close that hole. |
| `IDENTITY_NAMES_ADDRESS` | — | The IdentityNames **ERC1967 proxy** (the implementation changes on upgrade; the proxy is the one that emits) |
| `CHAIN_ID` | unset | Refuse to start unless the RPC reports this chain id |
| `CONFIRMATIONS` | `5` | Blocks behind the head to stay (shallow-reorg protection) |
| `POLL_INTERVAL_SECS` | `5` | Poll cadence, and the retry delay after a failure |
| `MAX_BLOCK_RANGE` | `10000` | Largest `eth_getLogs` window |
| `START_BLOCK` | unset | Where a FRESH scan starts — consulted only when no cursor exists (new database, or right after a re-index). Unset means the deployment block is found by binary search over `eth_getCode` and cached |
| `LISTEN_ADDR` | `127.0.0.1:8080` | Read-API listen address |

## API

| Endpoint | Answers |
|---|---|
| `GET /v1/resolve/handle/{platform}/{handle}` | The wallet a handle resolves to (`resolveHandle`), plus the account id it pairs with and whether the pair agrees (`resolvePair`) |
| `GET /v1/resolve/id/{platform}/{userId}` | The wallet an account id resolves to (`resolveId`), plus the handle that account currently holds |
| `GET /v1/resolve/address/{address}` | Every identity a wallet proved, with `resolves` and `published` flags (`primaryOf`'s reverse display) |
| `GET /v1/search?q=gre&platform=x&limit=10` | Matching variants for a partial handle: exact, then prefix, then substring, then trigram-fuzzy |
| `GET /v1/status` | Chain id, contract, last indexed block, chain head, lag, last window error, read-model version |
| `GET /health` | Liveness |

`{platform}` is a short key (`x`, `github`, `google`) or a 0x-hex 32-byte
platform id. With a known key, the handle in the path is normalized exactly
the way the chain normalized it before keying (`libid-identity`); `{userId}`
is always matched byte-verbatim, because the contract never normalizes ids.

## Read model

Schema `names`, all tables keyed by `chain_id` (one process follows one
chain; a second deployment can share the database):

- `events` — append-only journal of every decoded log, the audit trail
- `ids` — mirrors `byId` + `handleOfId`: account id → wallet, current handle
- `handles` — mirrors `byHandle` + `idOfHandle`: handle node → wallet;
  `owner NULL` mirrors the contract's retirement, and the `observed_at`
  watermark survives it the way the contract keeps it
- `published` — the display names; a row exists exactly while the contract's
  stored string is nonempty
- `platforms`, `verifiers` — operational metadata from the admin events

Each poll window commits in one transaction — journal, projections and cursor
together — and the journal's `(chain, block, log)` conflict gates the
projection writes, so a crash or an overlap replays a window and converges.
One process holds a per-chain Postgres advisory lock while it may write; a
second instance (a rolling-deploy overlap) blocks on it instead of
interleaving.

Bump `INDEXER_VERSION` (in `db.rs`) when the written shape changes: the next
start clears that chain's rows — co-tenant chains untouched — and replays it
from the deployment block. The re-index is the migration. Changing
`IDENTITY_NAMES_ADDRESS` triggers the same per-chain replay, because the old
contract's bindings are not the new contract's bindings.

## Deploying

Every push to `main` and every `v*.*.*` tag publishes a linux/amd64 image to
`ghcr.io/libid-org/usernames-indexer` (`:main`, `:latest`, `:<version>`,
`:sha-<commit>`); PRs build the image without pushing so packaging cannot
rot. The container binds `0.0.0.0:8080` and is configured entirely through
the environment (table above). Probes: `GET /health` for liveness; for
readiness gate on `GET /v1/status` — the resolve endpoints answer 503 by
design until the first window lands. The API sends permissive CORS for GET,
so a browser UI (handle.link) can call it directly from any origin.

`docker compose up -d --build` runs the full stack locally against the
compose Postgres — set `RPC_URL` and `IDENTITY_NAMES_ADDRESS` in `.env`
first.

## Caveats worth knowing

- **`PlatformConfigured` re-keying**: reconfiguring a platform's rules on
  chain re-keys every handle already written, and the event carries no rules
  payload. This build compiles its rules in from `handles.json` (via
  `libid-identity`). A reconfiguration is recorded and warned about; if the
  rules actually changed, string lookups need this build's rules updated and
  a re-index. Node-keyed lookups stay exact throughout.
- **Reorgs**: the loop stays `CONFIRMATIONS` blocks behind the head; there is
  no rollback. On a chain with deeper reorgs, raise the setting.
- **Unknown platforms** index fine (events are self-describing) but cannot be
  normalized or named by key on this side until `handles.json` learns them.
- **Sync state is a caller's concern**: until the first window commits, the
  resolution endpoints answer 503 rather than an authoritative-looking 404.
  After that, a replaying or lagging indexer serves what it has; gate on
  `/v1/status` (`lagBlocks`, `lastWindowError`) where staleness matters.
