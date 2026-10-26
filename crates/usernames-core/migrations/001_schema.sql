-- The read model for IdentityRegistry and HandleEscrow. Every statement is
-- re-appliable: the migration runner records versions, but nothing here
-- breaks if it runs twice.
--
-- Everything is keyed by chain_id. One indexer process follows one chain, any
-- number of them share a database, and the API reads across all of them.
-- Identities and handles are keyed by their nodes; handle text lives in
-- handle_nodes alone.

CREATE SCHEMA IF NOT EXISTS names;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- What the indexer records about a chain beside its rows: the cursor, the
-- head and the target it chases with the report's expiry, the contracts and
-- the Proof Verifier it saw, the read-model version, the cached deployment
-- block, the last window error. The API's status is read from here.
CREATE TABLE IF NOT EXISTS names.chain_metadata (
    chain_id BIGINT NOT NULL,
    key      TEXT   NOT NULL,
    value    TEXT   NOT NULL,
    PRIMARY KEY (chain_id, key)
);

-- Append-only journal of every decoded contract event, in chain order. The
-- projections below are derived state; this is the audit trail that says why
-- they hold what they hold, and its primary key is the idempotency gate for
-- every projection write.
CREATE TABLE IF NOT EXISTS names.events (
    chain_id     BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    log_index    BIGINT NOT NULL,
    tx_hash      BYTEA  NOT NULL,
    block_time   BIGINT NOT NULL,   -- unix seconds
    kind         TEXT   NOT NULL CHECK (kind IN (
        'identity_bound', 'handle_revealed', 'handle_published', 'handle_retired',
        'handle_unpublished', 'proof_verifier_configured', 'ceremony_bound',
        'bind_fee_paid', 'deposited', 'forwarded', 'claimed', 'refunded'
    )),
    -- The handle the event concerns, a ceremony's and a fee's included: what
    -- a handle history lists, from the first deposit to a handle nobody
    -- holds through every bind and claim after.
    handle_node          BYTEA,
    -- The event's parameters: each kind sets the ones it carries.
    platform_id          BYTEA,
    holder               BYTEA,
    id_node              BYTEA,
    observed_at          BIGINT,
    ceremony_version     INTEGER,
    verifier             BYTEA,
    authorization_digest BYTEA,
    client_identifier    BYTEA,
    receiver             BYTEA,
    token                BYTEA,
    sender               BYTEA,
    claimer              BYTEA,
    recipient            BYTEA,
    round                NUMERIC(78,0),
    amount               NUMERIC(78,0),
    released             NUMERIC(78,0),
    received             NUMERIC(78,0),
    refundable_at        BIGINT,
    PRIMARY KEY (chain_id, block_number, log_index)
);
CREATE INDEX IF NOT EXISTS events_handle_history_idx
    ON names.events (handle_node, block_time, chain_id, block_number, log_index)
    WHERE handle_node IS NOT NULL;

-- Mirrors idBindings plus handleNodeById: one row per id node ever bound. The
-- identity's current handle node rides along because the contract stores
-- that pointer next to the binding, and every reverse lookup wants it.
CREATE TABLE IF NOT EXISTS names.ids (
    chain_id         BIGINT      NOT NULL,
    id_node          BYTEA       NOT NULL,   -- 32 bytes
    platform_id      BYTEA       NOT NULL,   -- 32 bytes
    owner            BYTEA       NOT NULL,   -- 20 bytes
    observed_at      BIGINT      NOT NULL,   -- shared scale, as emitted
    ceremony_version BIGINT      NOT NULL,   -- logged by the contract, never stored there
    handle_node      BYTEA       NOT NULL,   -- handleNodeById
    block_number     BIGINT      NOT NULL,   -- provenance: the write that produced this row
    log_index        BIGINT      NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, id_node)
);
-- Reads ask by node or by holder, across every chain or one; chain_id trails
-- so the same index serves the scoped read and yields the per-chain order the
-- API returns.
CREATE INDEX IF NOT EXISTS ids_node_chain_idx
    ON names.ids (id_node, chain_id);
CREATE INDEX IF NOT EXISTS ids_owner_chain_idx
    ON names.ids (owner, chain_id);

-- Mirrors handleBindings plus idNodeByHandle: one row per handle node ever
-- bound. owner NULL mirrors the contract's address(0) after HandleRetired;
-- the observed_at watermark survives retirement, because the contract keeps
-- it to refuse older proofs from retaking the node.
CREATE TABLE IF NOT EXISTS names.handles (
    chain_id         BIGINT      NOT NULL,
    handle_node      BYTEA       NOT NULL,
    platform_id      BYTEA       NOT NULL,
    owner            BYTEA,                  -- NULL = retired, no longer resolves
    observed_at      BIGINT      NOT NULL,
    ceremony_version BIGINT      NOT NULL,   -- of the last binding at this node
    id_node          BYTEA       NOT NULL,   -- idNodeByHandle back-pointer
    block_number     BIGINT      NOT NULL,
    log_index        BIGINT      NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, handle_node)
);
CREATE INDEX IF NOT EXISTS handles_node_chain_idx
    ON names.handles (handle_node, chain_id);
-- A holder's handles in node order, so the slots they hold page in the
-- order of the escrow's key; retired rows never match.
CREATE INDEX IF NOT EXISTS handles_owner_chain_node_idx
    ON names.handles (owner, chain_id, handle_node) WHERE owner IS NOT NULL;

-- Every handle node an event named with its platform: a bind,
-- HandleRevealed, HandlePublished or HandleRetired. `handle` is the text a
-- HandleRevealed carried, folded and checked to hash to the node before it
-- is written, and kept whatever happens to the name or the handle after; a
-- bare deposit names no node here.
CREATE TABLE IF NOT EXISTS names.handle_nodes (
    chain_id    BIGINT NOT NULL,
    handle_node BYTEA  NOT NULL,
    platform_id BYTEA  NOT NULL,
    handle      TEXT,
    PRIMARY KEY (chain_id, handle_node)
);
-- Substring and fuzzy search: pg_trgm similarity and LIKE '%q%'.
CREATE INDEX IF NOT EXISTS handle_nodes_handle_trgm_idx
    ON names.handle_nodes USING gin (handle gin_trgm_ops);

-- Mirrors primaryHandles: each holder's one name, as its platform and the
-- node it hashes to, never its text. A row exists exactly while the
-- contract's entry is set: HandlePublished upserts, HandleUnpublished deletes.
CREATE TABLE IF NOT EXISTS names.published (
    chain_id    BIGINT      NOT NULL,
    owner       BYTEA       NOT NULL,
    platform_id BYTEA       NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    handle_node BYTEA       NOT NULL,
    PRIMARY KEY (chain_id, owner)
);

-- Each disclosed handle and the identity behind it: the id node, never an id.
-- id_node and holder are NULL until a bind takes the node.
CREATE OR REPLACE VIEW names.handle_identities AS
SELECT d.chain_id, d.platform_id, d.handle, d.handle_node, h.id_node,
       h.owner AS holder
FROM names.handle_nodes d
LEFT JOIN names.handles h
  ON h.chain_id = d.chain_id AND h.handle_node = d.handle_node
WHERE d.handle IS NOT NULL;

-- The names a chain goes by in an ENS name: the `base` in
-- `alice.x.base.handles.link`. Each chain's indexer writes its own from its
-- configuration at every start, replacing what it declared before. The
-- primary key is the name, not (chain_id, name): a name belongs to one chain
-- across the whole store, which is what lets a label mean one thing.
CREATE TABLE IF NOT EXISTS names.chain_names (
    name       TEXT        PRIMARY KEY,
    chain_id   BIGINT      NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS chain_names_chain_idx
    ON names.chain_names (chain_id);

-- Mirrors HandleEscrow's held and round: one row per (handle node, token)
-- that ever escrowed anything. Amounts are uint256, exact.
CREATE TABLE IF NOT EXISTS names.escrow_held (
    chain_id     BIGINT        NOT NULL,
    handle_node  BYTEA         NOT NULL,
    token        BYTEA         NOT NULL,   -- 20 bytes; EIP-7528 0xEeee…EEeE is the chain's coin
    held         NUMERIC(78,0) NOT NULL,   -- what `claim` would pay the holder now
    round        NUMERIC(78,0) NOT NULL,   -- claims so far: the round deposits book under
    block_number BIGINT        NOT NULL,   -- provenance: the write that produced this row
    log_index    BIGINT        NOT NULL,
    updated_at   TIMESTAMPTZ   NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, handle_node, token)
);
-- The unclaimed list, token by token, largest first, read backwards: across
-- every chain, and on one.
CREATE INDEX IF NOT EXISTS escrow_held_unclaimed_idx
    ON names.escrow_held (token, held, chain_id, handle_node) WHERE held > 0;
CREATE INDEX IF NOT EXISTS escrow_held_chain_unclaimed_idx
    ON names.escrow_held (chain_id, token, held, handle_node) WHERE held > 0;
-- One handle's slots still holding something, token by token.
CREATE INDEX IF NOT EXISTS escrow_held_node_idx
    ON names.escrow_held (handle_node, chain_id, token) WHERE held > 0;

-- Mirrors `contribution`: what each sender may take back from a slot, and
-- from when. A refund deletes its row; a claim closes the slot's round and
-- deletes all of them, the way it ends their refunds on chain, so every row
-- is in its slot's current round. Keyed by the address first, because that
-- is how it is read.
CREATE TABLE IF NOT EXISTS names.escrow_refundable (
    sender        BYTEA         NOT NULL,
    chain_id      BIGINT        NOT NULL,
    handle_node   BYTEA         NOT NULL,
    token         BYTEA         NOT NULL,
    amount        NUMERIC(78,0) NOT NULL,
    refundable_at BIGINT        NOT NULL,   -- unix seconds; the sender's last deposit sets it
    updated_at    TIMESTAMPTZ   NOT NULL DEFAULT now(),
    PRIMARY KEY (sender, chain_id, handle_node, token)
);
-- A claim closes one slot's round for every sender at once.
CREATE INDEX IF NOT EXISTS escrow_refundable_slot_idx
    ON names.escrow_refundable (handle_node, chain_id, token);

-- The addresses each journal row involves, one row per (event, address) with
-- every role the address plays in it: what an address history lists, newest
-- first. The key pages one chain; the index pages every chain.
CREATE TABLE IF NOT EXISTS names.address_events (
    chain_id     BIGINT NOT NULL,
    address      BYTEA  NOT NULL,
    block_time   BIGINT NOT NULL,   -- unix seconds
    block_number BIGINT NOT NULL,
    log_index    BIGINT NOT NULL,
    roles        TEXT[] NOT NULL,
    PRIMARY KEY (chain_id, address, block_time, block_number, log_index)
);
CREATE INDEX IF NOT EXISTS address_events_history_idx
    ON names.address_events (address, block_time, chain_id, block_number, log_index);

