-- Chain-scoped metadata gets a chain_id column of its own.
--
-- pipeline_metadata smuggled the chain (and, for deploy blocks, the
-- contract) into stringly keys — the one table that violated the schema's
-- own rule that everything is keyed by chain_id. With a real column,
-- clearing a chain's state is one DELETE that cannot forget a key, and
-- co-tenant chains are separated by structure instead of by string prefix.
--
-- Like 001, everything here is re-appliable: the migration runner records
-- versions, but nothing breaks if it runs twice — the carry-over is guarded
-- on the old table still existing.

CREATE TABLE IF NOT EXISTS names.chain_metadata (
    chain_id BIGINT NOT NULL,
    key      TEXT   NOT NULL,
    value    TEXT   NOT NULL,
    PRIMARY KEY (chain_id, key)
);

-- Carry over what the old keys held. The key names change with the table:
-- scoping used to live in the key, now it lives in the column. The
-- deployment-block cache keeps the contract in its key — the cache is per
-- (chain, contract), so repointing at a different contract must not inherit
-- the old contract's deployment block.
DO $$
BEGIN
    IF to_regclass('names.pipeline_metadata') IS NULL THEN
        RETURN; -- already migrated, or a database born after this migration
    END IF;

    INSERT INTO names.chain_metadata (chain_id, key, value)
    SELECT split_part(key, ':', 2)::BIGINT, 'cursor', value
    FROM names.pipeline_metadata WHERE key LIKE 'indexer_last_block:%'
    ON CONFLICT (chain_id, key) DO NOTHING;

    INSERT INTO names.chain_metadata (chain_id, key, value)
    SELECT split_part(key, ':', 2)::BIGINT, 'head', value
    FROM names.pipeline_metadata WHERE key LIKE 'chain_head:%'
    ON CONFLICT (chain_id, key) DO NOTHING;

    INSERT INTO names.chain_metadata (chain_id, key, value)
    SELECT split_part(key, ':', 2)::BIGINT, 'window_error', value
    FROM names.pipeline_metadata WHERE key LIKE 'last_window_error:%'
    ON CONFLICT (chain_id, key) DO NOTHING;

    INSERT INTO names.chain_metadata (chain_id, key, value)
    SELECT split_part(key, ':', 2)::BIGINT, 'schema_version', value
    FROM names.pipeline_metadata WHERE key LIKE 'indexer_schema_version:%'
    ON CONFLICT (chain_id, key) DO NOTHING;

    INSERT INTO names.chain_metadata (chain_id, key, value)
    SELECT split_part(key, ':', 2)::BIGINT, 'contract', value
    FROM names.pipeline_metadata WHERE key LIKE 'contract:%'
    ON CONFLICT (chain_id, key) DO NOTHING;

    INSERT INTO names.chain_metadata (chain_id, key, value)
    SELECT split_part(key, ':', 2)::BIGINT,
           'deploy_block:' || split_part(key, ':', 3),
           value
    FROM names.pipeline_metadata WHERE key LIKE 'deploy_block:%'
    ON CONFLICT (chain_id, key) DO NOTHING;
END
$$;

DROP TABLE IF EXISTS names.pipeline_metadata;
