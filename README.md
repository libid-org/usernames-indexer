# usernames-indexer

Indexes [`IdentityRegistry`](https://github.com/libid-org/libid-contracts/blob/main/solidity/contracts/identity/IdentityRegistry.sol)
and [`HandleEscrow`](https://github.com/libid-org/libid-contracts/blob/main/solidity/contracts/escrow/HandleEscrow.sol)
events into Postgres and serves resolution and search over the claimed
handles, the history of every address and handle, and what the escrow holds.
**Two binaries over one read model**: `usernames-indexer`, a loop that
follows the contracts' events — pushed by a log subscription, or polled —
and indexes their storage from those events alone, and `usernames-api`,
which serves what the loop wrote. They are separate because
they scale and fail differently — one writer per chain holds a Postgres
advisory lease, while readers are stateless and horizontal — and because a
stalled indexer must not hide behind a healthy-looking endpoint.

Neither binary contains logic: both stand on the `usernames-core` library,
where the read model, the event decoding and the loop itself live.

The contract was designed for exactly this: `IdentityBound` carries the
plaintext `id` and the normalized `handle` next to their storage nodes,
so the read model needs no on-chain strings, and the `published` flag plus
`HandleUnpublished` are emitted precisely so an off-chain index can reproduce
reverse display without guessing.

## Running

```sh
docker compose up -d postgres          # listens on 127.0.0.1:55432
cp .env.example .env                   # fill in RPC_URL and IDENTITY_NAMES_ADDRESS
cargo run -p usernames-indexer         # indexes the chain into the database
cargo run -p usernames-api             # serves the API, in another shell
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

Configuration is flags or environment (a `.env` file is read first);
durations are written the way humantime reads them: `5s`, `1500ms`, `2m`. Each
binary accepts only what it uses: the API takes no `RPC_URL` and none of the
indexer's settings.

| Variable | Used by | Default | Meaning |
|---|---|---|---|
| `DATABASE_URL` | both | — | Postgres connection string |
| `RPC_URL` | indexer | — | JSON-RPC endpoint of the chain to follow. With `LOG_SOURCE=subscribe` an `http(s)` URL is dialled as `ws(s)` on the same host and path, where Alchemy and a bare node serve their WebSocket. Its `eth_getLogs` must return `blockTimestamp` (reth, geth and anvil do); a log without one fails the window. Prefer a single node or a sticky endpoint: a load balancer that mixes lagged replicas can answer `eth_getLogs` for blocks a backend has not seen, and events dropped that way past the confirmation margin are gone until a re-index. The loop re-checks the backend's height before committing a window, which narrows but cannot close that hole. |
| `IDENTITY_NAMES_ADDRESS` | indexer | — | The IdentityRegistry **ERC1967 proxy** (the implementation changes on upgrade; the proxy is the one that emits). The indexer records it per chain, and `/v1/status` reports it from there |
| `HANDLE_ESCROW_ADDRESS` | indexer | unset | The HandleEscrow **ERC1967 proxy**, when the chain has one. The indexer refuses to start unless its `registry()` is `IDENTITY_NAMES_ADDRESS`. Its events share the registry's windows and cursor; setting, changing or unsetting it replays the chain. `/v1/status` reports it as `escrow` |
| `CHAIN_ID` | indexer | unset | Refuse to start unless the RPC reports this chain id. The API takes none: it serves every chain the store holds, and a request narrows with `?chain=` |
| `CHAIN_NAMES` | indexer | — | Required. The names this chain goes by in an ENS name, comma-separated: the `base` in `alice.x.base.handles.link`. Labels only, never a platform key. Written to the store at every start for the gateway to read; a name belongs to one chain across the store, and declaring one another chain holds refuses to start |
| `CONFIRMATIONS` | indexer | `5` | Blocks behind the head to stay (shallow-reorg protection). At least 1 with `LOG_SOURCE=subscribe` |
| `LOG_SOURCE` | indexer | `subscribe` | How the indexer learns of new logs: `subscribe` holds an `eth_subscribe("logs")` stream on a WebSocket; `poll` asks with `eth_getLogs` every `POLL_INTERVAL`, for an endpoint without WebSocket. See [Log sources](#log-sources) |
| `POLL_INTERVAL` | indexer | `5s` | The loop's cadence, and the retry delay after a failure: a poll cycle, or with a subscription a renewal of the report and, while pushed logs wait for their confirmations, a head read |
| `HEAD_INTERVAL` | indexer | `1m` | With a subscription, how often an idle indexer reads the head; each read carries the cursor past blocks without events |
| `STALE_AFTER` | indexer | four poll intervals + 1m | How long readers may trust this indexer's last report; the ENS gateway refuses this chain once it expires. Must exceed `POLL_INTERVAL`, and at most a year |
| `MAX_BLOCK_RANGE` | indexer | `10000` | Largest `eth_getLogs` window |
| `START_BLOCK` | indexer | unset | Where a FRESH scan starts — consulted only when no cursor exists (new database, or right after a re-index). Unset means the deployment block is found by binary search over `eth_getCode`; only a successful detection is cached, and an RPC failure mid-search retries next cycle |
| `LISTEN_ADDR` | api | `127.0.0.1:8080` | Read-API listen address |

### Log sources

**Subscribe** (the default) costs RPC calls per event rather than per
block. A session subscribes to both contracts' logs, reads the head, and
backfills from the cursor in `eth_getLogs` windows: the confirmed blocks
commit, and the blocks above `CONFIRMATIONS` wait beside the pushed logs.
Any socket drop, stream overflow or RPC error ends the session, and the
next one backfills from the cursor, so nothing the socket missed is lost.
A pushed log commits once its block is `CONFIRMATIONS` deep, after its
block hash is compared with the canonical block at that height; a block a
reorg replaced has its logs refetched by the new hash. Between events, a
head read every `HEAD_INTERVAL` carries the cursor forward, and the report
is renewed every `POLL_INTERVAL` from the database alone.

**Poll** asks for every window with `eth_getLogs` and re-reads the head
each `POLL_INTERVAL`, which costs RPC calls per block whether or not
anything happened.

## API

| Endpoint | Answers |
|---|---|
| `GET /v1/resolve/handle/{platform}/{handle}` | The wallet a handle resolves to (`resolveHandle`) on each chain it is bound on, each with the account id it pairs with |
| `GET /v1/resolve/id/{platform}/{userId}` | The wallet an account id resolves to (`resolveId`) on each chain it is bound on, each with the handle that account currently holds |
| `GET /v1/resolve/address/{address}` | Every identity a wallet proved on every chain, with `resolves` and `published` flags (`publishedHandleOf`'s reverse display) |
| `GET /v1/search?q=gre&platform=x&owner=0x…&limit=10&offset=0` | Live handles matching a partial query (exact, then prefix, then substring, then trigram-fuzzy), linked to a wallet, or both; one of `q` and `owner` is required. `limit` (1..50, default 10) and `offset` (up to 10000) page the ranked list; a page shorter than `limit` is the last |
| `GET /v1/history/address/{address}?before=&limit=` | Every event the address took part in, newest first, each with its `roles`: `holder`, `previousHandleHolder` and `previousIdHolder` (an event took that handle or account from it), `feeReceiver`, `depositor`, `refundTo` (a claim took its deposit, too), `claimer`, `recipient` |
| `GET /v1/history/handle/{platform}/{handle}?before=&limit=` | Every event on the handle: deposits while nobody held it, the binds that gave it a holder, and every claim, refund and payment after |
| `GET /v1/history/node/{node}` | The same, by handle node: for a handle nobody has bound, whose text no event carried |
| `GET /v1/escrow/address/{address}` | What waits for an address: `claimable`, held for the handles it holds now, and `refundable`, what deposits naming it as `refundTo` booked that nobody has claimed yet |
| `GET /v1/escrow/handle/{platform}/{handle}` | What a handle holds, token by token, and its holder; `/v1/escrow/node/{node}` by node |
| `GET /v1/escrow/unclaimed?token=0x…&platform=x&before=&limit=` | Every slot still holding something, bound or not: token by token, descending, the largest amount first within each |
| `GET /v1/status` | Every chain the store holds: chain id, contract, escrow, last indexed block, chain head, lag, when the indexer last reported and how long that report is still good, last window error, the Proof Verifier the contract is wired to; and the read-model version |
| `GET /health` | Liveness |

Every read spans every chain the store holds, and every result carries its
`chainId`; `?chain=8453` narrows a read to one chain. Nothing about chains
or contracts is configured on the API: the indexers wrote it.

A history is ordered by block time, then chain, block and log index, so one
spanning chains interleaves them in time. Histories and the unclaimed list page
by cursor: a page holds `limit` entries (1..100, default 20) and, while more
remain, a `next` to pass back as `before`. Each entry carries the event in `event`, tagged by `kind`
(the journal's kinds, fields named as the contract names them), and the
handle it concerns. Amounts and rounds are `uint256` decimal strings; the
chain's own coin is the EIP-7528 token `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE`.
Anybody can deposit a token they wrote, so show the tokens you recognize.

A deposit names a handle by its node, never its text. Until somebody binds
the handle, the store knows it by node and platform alone: `handle` is absent
on those amounts and entries. Asking by text works from the first deposit,
because the API hashes the text the way the chain does.

`{platform}` is a short key (`x`, `github`, `google`) or a 0x-hex 32-byte
platform id. With a known key, the handle in the path is normalized exactly
the way the chain normalized it before keying (`libid-identity`); `{userId}`
is always matched byte-verbatim, because the contract never normalizes ids.
A Google `{userId}` is the id the chain binds: `0x` and 64 lowercase hex
digits of SHA-256(`"libid.google-user-id" || sub`), not the `sub`.

Errors share one envelope — `{ "error": { "code", "message" } }`. The `code`
is stable and machine-readable; the prose is for humans and may be reworded.
Resolution 404s distinguish `handle_not_bound`, `handle_retired`,
`id_not_bound`, `platform_not_configured`, and `handle_impossible` (text the
platform could never hold); `not_synced` is the 503 before the first window;
bad input is `invalid_platform`, `invalid_address`, `invalid_node`,
`invalid_cursor`, or `invalid_argument`;
`internal` is a 500.

## ENS gateway

`usernames-api` also serves the ERC-3668 endpoint behind
[`HandleResolver`](https://github.com/libid-org/libid-contracts/blob/main/solidity/contracts/ens/HandleResolver.sol),
so a name in an X bio resolves in a wallet that has never heard of libID. It is
mounted only when `ENS_SIGNER_KEY` is set; unset, the route is absent rather
than present and failing.

```
GET /ens/{sender}/{data}.json   ->  { "data": "0x…" }
GET /ens/status                 ->  { "chains": [ { "chainId", "names", "lagBlocks",
                                      "indexerReportedAt", "reportValidFor",
                                      "ambiguous", "stale" } ] }
```

The resolver reverts `OffchainLookup` carrying this endpoint, the client fetches
the blob, and a second `eth_call` hands it back to `resolveWithProof`, which
verifies the signature and returns the record. Both on-chain halves are `view`.

| Variable | Default | Meaning |
|---|---|---|
| `ENS_SIGNER_KEY` | unset | A hex secp256k1 key, or an AWS KMS key id, alias (`alias/…`) or ARN, told apart by shape; with KMS the private material never enters the process, and region and credentials come from the ambient AWS chain (IRSA in the cluster). Setting it mounts the route; startup logs the signer address the resolver must trust |
| `ENS_RESOLVER_ADDRESS` | — | Required with a key. The one `HandleResolver` on the ENS chain; it answers for every indexed chain, since the request's coin type picks the chain. Every answer is signed for this address, whatever `{sender}` the path carries; a request naming another resolver is refused with a 400, so a value that fell behind a `setResolver` is a visible error rather than a signature the resolver rejects |
| `ENS_TTL` | `5m` | How long an answer stays good; the resolver enforces it. At most `55m`: the resolver's `MAX_LIFETIME` less five minutes for the chain to trail |
| `ENS_MAX_LAG_BLOCKS` | `32` | How far behind the chain the index may be and still assert anything. The target is set at the top of a cycle and the cursor catches up chunk by chunk, so this must exceed the blocks any served chain produces in one of its indexer's poll intervals |

**Null is an answer; stale is not.** A name nobody holds gets a *signed* null —
a wallet has to trust "nobody holds this" as much as it trusts an address, or
every unclaimed name looks like an outage. An index further behind than
`ENS_MAX_LAG_BLOCKS` gets an *unsigned* 503 instead, so the client falls through
to the next endpoint in the resolver's `urls`. Signing a null from a stale
index would assert the absence of a binding that may already exist.

**Two bounds, because one cannot see the other's failure.** `ENS_MAX_LAG_BLOCKS`
measures the cursor against the target — and both are the indexer's own
writes, so an indexer that stopped (crashed, lost its RPC, waiting on the
writer lease) freezes them together and reads as caught up for as long as it
stays down. So beside every target it sets, the indexer also declares how long
that report may be trusted (`STALE_AFTER`, four poll intervals plus a
minute unless set), renewing it per cycle and per committed chunk; the gateway
refuses a chain whose report has expired. The expiry is stamped and read back
with the database's clock, so no host's clock enters into it, and every
indexer sets its own, so slow chains and fast chains each expire on their own
schedule. Every row involved is keyed by chain: one chain's dead indexer
expires that chain alone, and the others keep answering.

`GET /ens/status` lists every served chain with its position and whether it
would be refused right now; `/v1/status` lists them as the indexers left
them. Both are
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
per request: nothing lists them, and an indexer for a new chain is discovered
— and gated on its own lag — from its first committed window. The resolver
carries ONE `urls` list for every
query it can answer; it cannot route by coin type. ERC-3668 has the client
walk that list until something succeeds, and a signed null is a success — so
a gateway that signed null for chains it does not hold would end the walk and
deny a binding the next endpoint existed to serve.

So the answers divide three ways: an address or a signed null for a chain in
the store, a signed null for a coin type naming no EVM chain at all, and an
unsigned 503 for an EVM chain the store does not hold. Only the last lets the
walk continue, which is exactly when it should. A chain's names in a name
(`alice.x.base.handles.link`) are the chain's own metadata: its indexer writes
them from `CHAIN_NAMES` at every start, replacing what it declared before, and
the gateway reads them per request. A name belongs to one chain across the
store — an indexer declaring a name another chain holds refuses to start — and
is never a platform key, so the parse stays unambiguous. A label naming no
chain in the store is a name nobody holds. To stop serving a chain, delete its
`names.chain_metadata` and `names.chain_names` rows (and its projection rows);
the gateway stops listing it on the next request. A database shared by
deployments is a served set shared by their gateways — give a gateway its own
database to narrow it.

**Coin types are matched, not decoded.** ENSIP-11 names a chain by
`0x80000000 | chainId`. Forwards that is exact for every chain id; backwards it
is not, because above 2³¹ the bit is already set and two chain ids land on one
coin type. So the gateway compares a query's coin type against the chains it
serves rather than computing a chain id from it.

That is what lets the eden testnet work: its chain id is 3735928814
(`0xDEADBFEE`), the OR leaves it unchanged, and a gateway that decoded would
have got 1588445166 and refused every eden name. The one genuinely ambiguous
store — holding 3735928814 AND 1588445166 together — is refused per request,
unsigned, so a wallet walks on rather than being answered with a guess;
`/ens/status` marks both rows `ambiguous`.

**Where answers come from.** The indexed model, and nothing else: the gateway
opens no RPC and takes no per-chain configuration. Answers are as of the
indexer's last committed window, kept `CONFIRMATIONS` blocks deep, which is
what `ENS_MAX_LAG_BLOCKS` and the indexer's own report guard.


**Tested against the real resolver.** `bin/usernames-api/tests/end_to_end.rs`
deploys the `HandleResolver` the `libid-contracts` crate ships on anvil and
walks the protocol as a wallet does:
`resolve` reverts `OffchainLookup`, the revert is decoded, the gateway signs
the answer in process, and `resolveWithProof` on chain turns it back into an
address. A second case signs with a key the resolver does not trust and
asserts the contract's own `UntrustedSigner` — so the passing case proves the
chain checked something rather than that something returned bytes.

**Addresses without a name.** A Google address is bound as proved, and its
alphabet is wider than a label's: `_` and `+` are both legal in an address and
neither can appear in an ENS label. No substitution is available — unlike X,
where `_` maps to `-` because X forbids `-`, a Google address may hold both, so
the map would not be reversible, and an irreversible map on a payment path is
worse than no name. Such accounts are reachable by address, not by name.

The design's id-derived fallback (`<idNode as 64 hex>._id.handles.link`) is not
implemented: 64 characters is one past the DNS label ceiling of RFC 1035, so
no standard client can encode it — ethers' `dnsEncode` refuses above 63. It
could not have covered these accounts, or any others.

## Read model

Schema `names`, all tables keyed by `chain_id` (one process follows one
chain; a second deployment can share the database):

- `events` — append-only journal of every decoded log, the audit trail.
  `CeremonyBound` and `BindFeePaid` live only here: which client
  authenticated a binding and what fee it paid are an operator's questions,
  and nothing resolves by them
- `ids` — mirrors `idBindings` + `handleNodeById`: id → holder, current handle
- `handles` — mirrors `handleBindings` + `idNodeByHandle`: handle node → holder;
  `owner NULL` mirrors the contract's retirement, and the `observed_at`
  watermark survives it the way the contract keeps it
- `published` — the display names; a row exists exactly while the contract's
  stored string is nonempty
- `platforms` — one row per platform the contract configured; the Proof
  Verifier it is wired to is chain metadata, reported by `/v1/status`
- `escrow_held` — mirrors HandleEscrow's `held` and `round` per handle node
  and token, with the platform its first deposit named
- `escrow_refundable` — mirrors `refundable`: each `refundTo`'s contribution
  in a slot's current round; a refund deletes its row, a claim the round's
- `address_events`, `handle_events` — which addresses (with their roles) and
  which handle each journal row involves, dated by its block. A bind's
  previous holders, a ceremony's and a fee's handle, and a fee's payer are
  read from the rows the event changes, or from the events its transaction
  emitted just before it

Each window commits in one transaction — journal, projections and cursor
together — and the journal's `(chain, block, log)` conflict gates the
projection writes, so a crash or an overlap replays a window and converges.
One process holds a per-chain Postgres advisory lock while it may write; a
second instance (a rolling-deploy overlap) blocks on it instead of
interleaving.

Bump `INDEXER_VERSION` (in `db.rs`) when the written shape changes: the next
start clears that chain's rows — co-tenant chains untouched — and replays it
from the deployment block. The re-index is the migration. Changing
`IDENTITY_NAMES_ADDRESS` or `HANDLE_ESCROW_ADDRESS` triggers the same
per-chain replay, because the old contract's bindings are not the new
contract's bindings.

## Deploying

Every push to `main` and every `v*.*.*` tag publishes **two** linux/amd64
images (`:main`, `:latest`, `:<version>`, `:sha-<commit>`); PRs build both
without pushing so packaging cannot rot:

| Image | Runs | Listens | Needs |
|---|---|---|---|
| `ghcr.io/libid-org/usernames-indexer` | the indexing loop | nothing | RPC + a writer lease |
| `ghcr.io/libid-org/usernames-api` | the read API | `0.0.0.0:8080` | the database only |

No image carries both: an entrypoint would have to default to one of them,
and a deployment that pulled it expecting the other would run a container
that looks healthy while doing half the job.

**Upgrading from the single-image build:** the `usernames-indexer` image keeps
its name and keeps indexing, but it no longer serves the API. Deploy
`usernames-api` alongside it, pointed at the same database; it needs nothing
else. Nothing in the database changes and no re-index is needed.

**Upgrading from 0.2:** the journal kind of the fee event is `bind_fee_paid`,
declared in `001_schema.sql`, so the indexer refuses a database 0.2 migrated
(`migration 1 was previously applied but has been modified`). Start it on a
fresh database, or stop it and run
`DROP SCHEMA names CASCADE; DROP TABLE _sqlx_migrations;` first; that keeps
`pg_trgm`, which a least-privilege role cannot create. Every chain then
replays from its contract's deployment block.

**Upgrading from 0.3:** `IdentityRegistry` 0.15 is a fresh deployment, and
`001_schema.sql` admits its `handle_unpublished` journal kind, so the indexer
refuses a database 0.3 migrated, as above. Start it on a fresh database with
`IDENTITY_NAMES_ADDRESS` set to the 0.15 registry; it indexes from that
registry's deployment block.

**Upgrading from 0.4:** roll the indexer first. It migrates the database in
place (`002_escrow_and_history.sql`) and, because `INDEXER_VERSION` is 2,
replays every chain from its registry's deployment block, filling the escrow
books and the histories. Set `HANDLE_ESCROW_ADDRESS` before that start, or the
chain replays again when it is set. A 0.4 API keeps serving its routes over the
migrated database; a 0.5 API started before the migration answers `internal`
on the new routes until it lands. A 0.4 indexer refuses a database 002 has
migrated: to go back, start it on a fresh database, or run
`DROP SCHEMA names CASCADE; DROP TABLE _sqlx_migrations;` first, as above.

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
compose Postgres — set `RPC_URL` and `IDENTITY_NAMES_ADDRESS` in
`.env` first.

## Caveats worth knowing

- **`PlatformConfigured` re-keying**: reconfiguring a platform's rules on
  chain re-keys every handle already written, and the event carries no rules
  payload. This build compiles its rules in from `handles.json` (via
  `libid-identity`). A reconfiguration is recorded and warned about; if the
  rules actually changed, string lookups need this build's rules updated and
  a re-index. Node-keyed lookups stay exact throughout.
- **Reorgs**: the loop commits nothing above `CONFIRMATIONS` blocks behind
  the head, and a subscription checks each block it pushed logs from against
  the canonical chain before committing it; there is no rollback. On a chain
  with deeper reorgs, raise the setting.
- **A subscription trusts its stream**: a head read proves the chain grew,
  not that the endpoint pushed every log of the blocks it covers. A log an
  endpoint drops on a live socket is missed until a re-index; a dropped
  socket loses nothing, because the next session backfills.
- **Unknown platforms** index fine (events are self-describing) but cannot be
  normalized or named by key on this side until `handles.json` learns them.
- **Sync state is a caller's concern**: until the first window commits, the
  resolution endpoints answer 503 rather than an authoritative-looking 404.
  After that, a replaying or lagging indexer serves what it has; watch
  `/v1/status` (`lagBlocks`, `reportValidFor`, `lastWindowError`) where staleness matters.
