-- The read model for IdentityNames. Every statement is re-appliable: the
-- migration runner records versions, but nothing here breaks if it runs twice.
--
-- Everything is keyed by chain_id. One indexer process follows one chain, but
-- the schema does not assume it stays that way: a second deployment pointed at
-- a second chain can share the database without a migration.

CREATE SCHEMA IF NOT EXISTS names;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Cursor, schema version, and the cached deployment block live here.
CREATE TABLE IF NOT EXISTS names.pipeline_metadata (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Append-only journal of every decoded contract event, in chain order. The
-- projections below are derived state; this is the audit trail that says why
-- they hold what they hold.
CREATE TABLE IF NOT EXISTS names.events (
    chain_id     BIGINT      NOT NULL,
    block_number BIGINT      NOT NULL,
    log_index    BIGINT      NOT NULL,
    tx_hash      BYTEA       NOT NULL,
    kind         TEXT        NOT NULL CHECK (kind IN (
        'identity_bound', 'handle_retired', 'name_unpublished',
        'platform_configured', 'verifier_configured', 'verifier_retired',
        'latest_version_changed'
    )),
    payload      JSONB       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, block_number, log_index)
);

-- Mirrors byId plus handleOfId: one row per account id ever bound. The
-- account's current handle node rides along because the contract stores that
-- pointer next to the binding, and every reverse lookup wants it.
CREATE TABLE IF NOT EXISTS names.ids (
    chain_id     BIGINT      NOT NULL,
    id_node      BYTEA       NOT NULL,   -- 32 bytes
    platform_id  BYTEA       NOT NULL,   -- 32 bytes
    user_id      TEXT        NOT NULL,   -- plaintext, byte-verbatim from the event
    owner        BYTEA       NOT NULL,   -- 20 bytes
    observed_at  BIGINT      NOT NULL,   -- shared scale, as emitted
    version      BIGINT      NOT NULL,
    handle_node  BYTEA       NOT NULL,   -- handleOfId
    block_number BIGINT      NOT NULL,   -- provenance: the write that produced this row
    log_index    BIGINT      NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, id_node)
);
CREATE INDEX IF NOT EXISTS ids_owner_idx
    ON names.ids (chain_id, owner);
CREATE INDEX IF NOT EXISTS ids_platform_user_idx
    ON names.ids (chain_id, platform_id, user_id);

-- Mirrors byHandle plus idOfHandle: one row per handle node ever bound.
-- owner NULL mirrors the contract's address(0) after HandleRetired; the
-- observed_at watermark is deliberately KEPT on retirement, because the
-- contract keeps it to refuse older proofs from retaking the node.
CREATE TABLE IF NOT EXISTS names.handles (
    chain_id     BIGINT      NOT NULL,
    handle_node  BYTEA       NOT NULL,
    platform_id  BYTEA       NOT NULL,
    handle       TEXT        NOT NULL,   -- normalized plaintext from the event
    owner        BYTEA,                  -- NULL = retired, no longer resolves
    observed_at  BIGINT      NOT NULL,
    version      BIGINT      NOT NULL,   -- 0 after retirement, like the contract
    id_node      BYTEA       NOT NULL,   -- idOfHandle back-pointer
    block_number BIGINT      NOT NULL,
    log_index    BIGINT      NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, handle_node)
);
CREATE INDEX IF NOT EXISTS handles_owner_idx
    ON names.handles (chain_id, owner) WHERE owner IS NOT NULL;
-- Prefix search: LIKE 'q%' is sargable through text_pattern_ops.
CREATE INDEX IF NOT EXISTS handles_platform_handle_idx
    ON names.handles (chain_id, platform_id, handle text_pattern_ops);
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
    platform_id BYTEA      NOT NULL,
    handle      TEXT        NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, owner, platform_id)
);

-- One row per platform id the contract has ever mentioned. platform_key is
-- set when the id matches a domain this build knows from handles.json; an
-- unknown platform still indexes, it just cannot be normalized or searched
-- by key. configured_count > 1 means setPlatform ran again, which re-keys
-- every handle on the platform — see the README on what that entails.
CREATE TABLE IF NOT EXISTS names.platforms (
    chain_id              BIGINT      NOT NULL,
    platform_id           BYTEA       NOT NULL,
    platform_key          TEXT,
    latest_version        BIGINT,
    configured_count      BIGINT      NOT NULL DEFAULT 0,
    last_configured_block BIGINT,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, platform_id)
);

-- Verifier wiring per (platform, proof version): operational metadata that
-- answers "which formats are live" without an RPC.
CREATE TABLE IF NOT EXISTS names.verifiers (
    chain_id               BIGINT      NOT NULL,
    platform_id            BYTEA       NOT NULL,
    version                BIGINT      NOT NULL,
    verifier               BYTEA       NOT NULL,
    max_future_observation BIGINT      NOT NULL,
    retired                BOOLEAN     NOT NULL DEFAULT false,
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, platform_id, version)
);
