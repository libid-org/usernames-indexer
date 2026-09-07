# usernames-indexer

Indexes [`IdentityNames`](https://github.com/libid-org/libid-contracts/blob/main/solidity/contracts/identity/IdentityNames.sol)
events into Postgres and serves resolution and search over the claimed
handles. **Two binaries over one read model**: `usernames-indexer`, a polling
loop that mirrors the contract's storage from its events alone, and
`usernames-api`, which serves what the loop wrote. They are separate because
they scale and fail differently — one writer per chain holds a Postgres
advisory lease, while readers are stateless and horizontal — and because a
stalled indexer must not hide behind a healthy-looking endpoint.

Neither binary contains logic: both stand on the `usernames-core` library,
where the read model, the event decoding and the loop itself live.

The contract was designed for exactly this: `IdentityBound` carries the
plaintext `userId` and the normalized `handle` next to their storage nodes,
so the read model needs no on-chain strings, and the `published` flag plus
`NameUnpublished` are emitted precisely so an off-chain mirror can reproduce
reverse display without guessing.

## Running

```sh
docker compose up -d postgres          # listens on 127.0.0.1:55432
cp .env.example .env                   # fill in RPC_URL, IDENTITY_NAMES_ADDRESS, CHAIN_ID
cargo run -p usernames-indexer         # the write half
cargo run -p usernames-api             # the read half, in another shell
```

Start the indexer first on a fresh database: it owns the schema and runs the
migrations. The API only connects, and a reader that starts before the schema
exists fails its first query rather than racing the migration.

The compose database's DSN is
`postgres://usernames:usernames_dev@127.0.0.1:55432/usernames` (already in
`.env.example`). The migrations run `CREATE EXTENSION pg_trgm`, which needs a
role allowed to create extensions — true for the compose database; on a
managed instance with a least-privilege role, have an administrator run it
once beforehand.

Configuration is flags or environment (a `.env` file is read first). Each
binary accepts only what it uses — the API takes no `RPC_URL`, and refusing
the indexer's knobs is the point rather than an omission:

| Variable | Used by | Default | Meaning |
|---|---|---|---|
| `DATABASE_URL` | both | — | Postgres connection string |
| `RPC_URL` | indexer | — | JSON-RPC endpoint of the chain to follow. Prefer a single node or a sticky endpoint: a load balancer that mixes lagged replicas can answer `eth_getLogs` for blocks a backend has not seen, and events dropped that way past the confirmation margin are gone until a re-index. The loop re-checks the backend's height before committing a window, which narrows but cannot close that hole. |
| `IDENTITY_NAMES_ADDRESS` | both | — | The IdentityNames **ERC1967 proxy** (the implementation changes on upgrade; the proxy is the one that emits). The API echoes it in `/v1/status`; set it to the same value the indexer runs with, or the status names a contract those rows did not come from |
| `CHAIN_ID` | both | unset / **required** | Indexer: refuse to start unless the RPC reports this chain id. API: **required** — it talks to no chain, so it cannot discover which chain's rows it serves |
| `CONFIRMATIONS` | indexer | `5` | Blocks behind the head to stay (shallow-reorg protection) |
| `POLL_INTERVAL_SECS` | indexer | `5` | Poll cadence, and the retry delay after a failure |
| `STALE_AFTER_SECS` | indexer | four poll intervals + 60 | How long readers may trust this indexer's last report; the ENS gateway refuses this chain once it expires. Must exceed `POLL_INTERVAL_SECS`, and at most a year |
| `MAX_BLOCK_RANGE` | indexer | `10000` | Largest `eth_getLogs` window |
| `START_BLOCK` | indexer | unset | Where a FRESH scan starts — consulted only when no cursor exists (new database, or right after a re-index). Unset means the deployment block is found by binary search over `eth_getCode`; only a successful detection is cached, and an RPC failure mid-search retries next cycle |
| `LISTEN_ADDR` | api | `127.0.0.1:8080` | Read-API listen address |

## API

| Endpoint | Answers |
|---|---|
| `GET /v1/resolve/handle/{platform}/{handle}` | The wallet a handle resolves to (`resolveHandle`), plus the account id it pairs with and whether the pair agrees (`resolvePair`) |
| `GET /v1/resolve/id/{platform}/{userId}` | The wallet an account id resolves to (`resolveId`), plus the handle that account currently holds |
| `GET /v1/resolve/address/{address}` | Every identity a wallet proved, with `resolves` and `published` flags (`primaryOf`'s reverse display) |
| `GET /v1/search?q=gre&platform=x&limit=10` | Matching variants for a partial handle: exact, then prefix, then substring, then trigram-fuzzy |
| `GET /v1/status` | Chain id, contract, last indexed block, chain head, lag, when the indexer last reported and how long that report is still good, last window error, read-model version |
| `GET /health` | Liveness |

`{platform}` is a short key (`x`, `github`, `google`) or a 0x-hex 32-byte
platform id. With a known key, the handle in the path is normalized exactly
the way the chain normalized it before keying (`libid-identity`); `{userId}`
is always matched byte-verbatim, because the contract never normalizes ids.

Errors share one envelope — `{ "error": { "code", "message" } }`. The `code`
is stable and machine-readable; the prose is for humans and may be reworded.
Resolution 404s distinguish `handle_not_bound`, `handle_retired`,
`id_not_bound`, `platform_not_configured`, and `handle_impossible` (text the
platform could never hold); `not_synced` is the 503 before the first window;
bad input is `invalid_platform`, `invalid_address`, or `invalid_argument`;
`internal` is a 500.

## ENS gateway

`usernames-api` also serves the ERC-3668 endpoint behind
[`HandleResolver`](https://github.com/libid-org/libid-contracts/blob/main/solidity/contracts/ens/HandleResolver.sol),
so a name in an X bio resolves in a wallet that has never heard of libID. It is
mounted only when `ENS_SIGNER_KEY` is set; unset, the route is absent rather
than present and failing.

```
GET /ens/{sender}/{data}.json   ->  { "data": "0x…" }
GET /ens/status                 ->  { "chains": [ { "chainId", "label", "source",
                                      "lagBlocks", "indexerReportedAt",
                                      "reportValidFor", "stale" } ] }
```

The resolver reverts `OffchainLookup` carrying this endpoint, the client fetches
the blob, and a second `eth_call` hands it back to `resolveWithProof`, which
verifies the signature and returns the record. Both on-chain halves are `view`.

| Variable | Default | Meaning |
|---|---|---|
| `ENS_SIGNER_KEY` | unset | The signing key, hex. Setting it mounts the route |
| `ENS_RESOLVER_ADDRESS` | — | Required with a key. Every answer is signed for this address, whatever `{sender}` the path carries; a request naming another resolver is refused with a 400, so a value that fell behind a `setResolver` is a visible error rather than a signature the resolver rejects |
| `ENS_TTL_SECS` | `300` | How long an answer stays good; the resolver enforces it |
| `ENS_MAX_LAG_BLOCKS` | `32` | How far behind the chain the mirror may be and still assert anything. The target is set at the top of a cycle and the cursor catches up chunk by chunk, so this must exceed the blocks any served chain produces in one of its indexer's poll intervals |

**Null is an answer; stale is not.** A name nobody holds gets a *signed* null —
a wallet has to trust "nobody holds this" as much as it trusts an address, or
every unclaimed name looks like an outage. A mirror further behind than
`ENS_MAX_LAG_BLOCKS` gets an *unsigned* 503 instead, so the client falls through
to the next endpoint in the resolver's `urls`. Signing a null from a stale
mirror would assert the absence of a binding that may already exist.

**Two bounds, because one cannot see the other's failure.** `ENS_MAX_LAG_BLOCKS`
measures the cursor against the target — and both are the indexer's own
writes, so an indexer that stopped (crashed, lost its RPC, waiting on the
writer lease) freezes them together and reads as caught up for as long as it
stays down. So beside every target it sets, the indexer also declares how long
that report may be trusted (`STALE_AFTER_SECS`, four poll intervals plus a
minute unless set), renewing it per cycle and per committed chunk; the gateway
refuses a chain whose report has expired. The expiry is stamped and read back
with the database's clock, so no host's clock enters into it, and every
indexer sets its own, so slow chains and fast chains each expire on their own
schedule. Every row involved is keyed by chain: one chain's dead indexer
expires that chain alone, and the others keep answering.

`GET /ens/status` lists every served chain with its position and whether it
would be refused right now; `/v1/status` reports the API's own chain. Both are
for alerting. A readiness probe may look at the STATUS CODE of `/v1/status`,
which is 200 whenever the database answers, but never at a chain's `stale`,
`reportValidFor` or `lagBlocks`: one stopped indexer would take every chain
out of rotation, when the gate already refuses that chain alone. And the
report says only that the loop is alive and can see the head. Progress is
still what `ENS_MAX_LAG_BLOCKS` measures, and `lastWindowError` is the alert
for a loop that is up but failing its windows.

**Upgrading across this change:** the report is the indexer's write, so deploy
the indexer image first, or together. A gateway on this version refuses, with
a 503, a chain whose indexer has never written one — that chain alone, and
only until its next cycle lands.

**One gateway, every chain in the store — and a refusal for the rest.** The
gateway serves whatever chains indexers have written into the database, read
per request: nothing lists them, and an indexer for a new chain is answered
for the first time it commits. The resolver carries ONE `urls` list for every
query it can answer; it cannot route by coin type. ERC-3668 has the client
walk that list until something succeeds, and a signed null is a success — so
a gateway that signed null for chains it does not hold would end the walk and
deny a binding the next endpoint existed to serve.

So the answers divide three ways: an address or a signed null for a chain in
the store, a signed null for a coin type naming no EVM chain at all, and an
unsigned 503 for an EVM chain the store does not hold. Only the last lets the
walk continue, which is exactly when it should. A chain's label in a name
(`alice.x.base.handles.link`) comes from one table in `usernames-core`, the
closed set the grammar needs — not from configuration.

**Coin types are matched, not decoded.** ENSIP-11 names a chain by
`0x80000000 | chainId`. Forwards that is exact for every chain id; backwards it
is not, because above 2³¹ the bit is already set and two chain ids land on one
coin type. So the gateway compares a query's coin type against the chains it
serves rather than computing a chain id from it.

That is what lets the eden testnet work: its chain id is 3735928814
(`0xDEADBFEE`), the OR leaves it unchanged, and a gateway that decoded would
have got 1588445166 and refused every eden name. The one genuinely ambiguous
store — holding 3735928814 AND 1588445166 together — is refused per request,
unsigned, so a wallet walks on rather than being answered with a guess.

**Where answers come from.** The indexed model, and nothing else: the gateway
opens no RPC and takes no per-chain configuration. Answers are as of the
indexer's last committed window, kept `CONFIRMATIONS` blocks deep, which is
what `ENS_MAX_LAG_BLOCKS` and the indexer's own report guard.


**Tested against the real resolver.** `bin/usernames-api/tests/end_to_end.rs`
deploys `HandleResolver` on anvil and walks the protocol as a wallet does:
`resolve` reverts `OffchainLookup`, the revert is decoded, the gateway signs
the answer in process, and `resolveWithProof` on chain turns it back into an
address. A second case signs with a key the resolver does not trust and
asserts the contract's own `UntrustedSigner` — so the passing case proves the
chain checked something rather than that something returned bytes.

Regenerating the resolver bytecode the test deploys is described in
[`contracts/README.md`](contracts/README.md); a drifted copy is a test that
passes against a resolver nobody deploys.

**Addresses without a name.** A Google address is bound as proved, and its
alphabet is wider than a label's: `_` and `+` are both legal in an address and
neither can appear in an ENS label. No substitution is available — unlike X,
where `_` maps to `-` because X forbids `-`, a Google address may hold both, so
the map would not be reversible, and an irreversible map on a payment path is
worse than no name. Such accounts are reachable by address, not by name.

The design's id-derived fallback (`<idNode as 64 hex>._id.handles.link`) is not
implemented, and deliberately: 64 characters is one past the DNS label ceiling
of RFC 1035, so no standard client can encode it — ethers' `dnsEncode` refuses
above 63. It could not have covered these accounts, or any others.

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

Every push to `main` and every `v*.*.*` tag publishes **two** linux/amd64
images (`:main`, `:latest`, `:<version>`, `:sha-<commit>`); PRs build both
without pushing so packaging cannot rot:

| Image | Runs | Listens | Needs |
|---|---|---|---|
| `ghcr.io/libid-org/usernames-indexer` | the polling loop | nothing | RPC + a writer lease |
| `ghcr.io/libid-org/usernames-api` | the read API | `0.0.0.0:8080` | the database only |

There is deliberately no image carrying both. An entrypoint would have to
default to one half, and a deployment that pulled it expecting the other would
run a container that looks healthy while doing half the job.

**Upgrading from the single-image build:** the `usernames-indexer` image keeps
its name and keeps indexing, but it no longer serves the API. Deploy
`usernames-api` alongside it, pointed at the same database and the same
`IDENTITY_NAMES_ADDRESS`, with `CHAIN_ID` set — the reader cannot discover the
chain on its own. Nothing in the database changes and no re-index is needed.

Probes belong to the API: `GET /health` for liveness; for readiness gate on
the status code of `GET /v1/status`, which is 200 whenever the database
answers — the resolve endpoints answer 503 by design until the first window
lands. It sends permissive CORS for GET, so a browser UI (handle.link) can
call it directly from any origin. The indexer exposes no port; supervise it on
process liveness, and alert on `lagBlocks` and `reportValidFor` from the API's
status — the first cannot move once the loop stops, the second counts down
exactly then — or on `/ens/status` for every chain the gateway serves. Never
fail readiness on those fields: one stopped indexer would take every chain
out of rotation.

`docker compose up -d --build` runs the full stack locally against the
compose Postgres — set `RPC_URL`, `IDENTITY_NAMES_ADDRESS` and `CHAIN_ID` in
`.env` first.

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
  After that, a replaying or lagging indexer serves what it has; watch
  `/v1/status` (`lagBlocks`, `reportValidFor`, `lastWindowError`) where staleness matters.
