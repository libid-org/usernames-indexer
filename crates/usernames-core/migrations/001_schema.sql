-- The read model for IdentityNames. Every statement is re-appliable: the
-- migration runner records versions, but nothing here breaks if it runs twice.
--
-- Everything is keyed by chain_id. One indexer process follows one chain, any
-- number of them share a database, and the API reads across all of them.

CREATE SCHEMA IF NOT EXISTS names;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- What the indexer records about a chain beside its rows: the cursor, the
-- head and the target it chases with the report's expiry, the contract and
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
    chain_id     BIGINT      NOT NULL,
    block_number BIGINT      NOT NULL,
    log_index    BIGINT      NOT NULL,
    tx_hash      BYTEA       NOT NULL,
    kind         TEXT        NOT NULL CHECK (kind IN (
        'identity_bound', 'handle_retired', 'name_unpublished',
        'platform_configured', 'proof_verifier_configured',
        'ceremony_bound', 'claim_fee_paid'
    )),
    payload      JSONB       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, block_number, log_index)
);

-- Mirrors byId plus handleOfId: one row per account id ever bound. The
-- account's current handle node rides along because the contract stores that
-- pointer next to the binding, and every reverse lookup wants it.
CREATE TABLE IF NOT EXISTS names.ids (
    chain_id         BIGINT      NOT NULL,
    id_node          BYTEA       NOT NULL,   -- 32 bytes
    platform_id      BYTEA       NOT NULL,   -- 32 bytes
    user_id          TEXT        NOT NULL,   -- plaintext, byte-verbatim from the event
    owner            BYTEA       NOT NULL,   -- 20 bytes
    observed_at      BIGINT      NOT NULL,   -- shared scale, as emitted
    ceremony_version BIGINT      NOT NULL,   -- logged by the contract, never stored there
    handle_node      BYTEA       NOT NULL,   -- handleOfId
    block_number     BIGINT      NOT NULL,   -- provenance: the write that produced this row
    log_index        BIGINT      NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, id_node)
);
-- Reads ask by (platform, id) or by wallet, across every chain or one;
-- chain_id trails so the same index serves the scoped read and yields the
-- per-chain order the API returns.
CREATE INDEX IF NOT EXISTS ids_platform_user_chain_idx
    ON names.ids (platform_id, user_id, chain_id);
CREATE INDEX IF NOT EXISTS ids_owner_chain_idx
    ON names.ids (owner, chain_id);

-- Mirrors byHandle plus idOfHandle: one row per handle node ever bound.
-- owner NULL mirrors the contract's address(0) after HandleRetired; the
-- observed_at watermark is deliberately KEPT on retirement, because the
-- contract keeps it to refuse older proofs from retaking the node.
CREATE TABLE IF NOT EXISTS names.handles (
    chain_id         BIGINT      NOT NULL,
    handle_node      BYTEA       NOT NULL,
    platform_id      BYTEA       NOT NULL,
    handle           TEXT        NOT NULL,   -- normalized plaintext from the event
    owner            BYTEA,                  -- NULL = retired, no longer resolves
    observed_at      BIGINT      NOT NULL,
    ceremony_version BIGINT      NOT NULL,   -- of the last binding at this node
    id_node          BYTEA       NOT NULL,   -- idOfHandle back-pointer
    block_number     BIGINT      NOT NULL,
    log_index        BIGINT      NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, handle_node)
);
-- Equality on (platform, handle), and prefix search through text_pattern_ops.
CREATE INDEX IF NOT EXISTS handles_platform_handle_chain_idx
    ON names.handles (platform_id, handle text_pattern_ops, chain_id);
-- The handles a wallet holds; retired rows never match a search.
CREATE INDEX IF NOT EXISTS handles_owner_chain_idx
    ON names.handles (owner, chain_id) WHERE owner IS NOT NULL;
-- Substring and fuzzy search: pg_trgm similarity and LIKE '%q%'.
CREATE INDEX IF NOT EXISTS handles_handle_trgm_idx
    ON names.handles USING gin (handle gin_trgm_ops);

-- Mirrors the published mapping: the handle a wallet chose to display.
-- A row exists exactly while the contract's stored string is nonempty, so
-- IdentityBound with published=true upserts and both published=false and
-- NameUnpublished delete.
CREATE TABLE IF NOT EXISTS names.published (
    chain_id    BIGINT      NOT NULL,
    owner       BYTEA       NOT NULL,
    platform_id BYTEA       NOT NULL,
    handle      TEXT        NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, owner, platform_id)
);

-- One row per platform id the contract has ever configured. platform_key is
-- set when the id matches a domain this build knows from handles.json; an
-- unknown platform still indexes, it just cannot be normalized or searched
-- by key. configured_count > 1 means setPlatform ran again, which re-keys
-- every handle on the platform — see the README on what that entails.
CREATE TABLE IF NOT EXISTS names.platforms (
    chain_id              BIGINT      NOT NULL,
    platform_id           BYTEA       NOT NULL,
    platform_key          TEXT,
    configured_count      BIGINT      NOT NULL DEFAULT 0,
    last_configured_block BIGINT,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, platform_id)
);
CREATE INDEX IF NOT EXISTS platforms_platform_idx
    ON names.platforms (platform_id);

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
