-- Hashed identities: the chain carries nodes, and handle text only where a
-- holder or a depositor disclosed it. Rows an earlier build wrote hold
-- plaintext ids and handles, so they go here; INDEXER_VERSION 3 replays every
-- chain from its registry's deployment block. The cursors go too, so the API
-- answers not_synced until a chain's replay lands.

TRUNCATE names.events, names.ids, names.handles, names.published,
         names.platforms, names.handle_nodes, names.escrow_held,
         names.escrow_refundable, names.address_events, names.handle_events;
DELETE FROM names.chain_metadata WHERE key = 'cursor';

ALTER TABLE names.events DROP CONSTRAINT IF EXISTS events_kind_check;
ALTER TABLE names.events ADD CONSTRAINT events_kind_check CHECK (kind IN (
    'identity_bound', 'handle_published', 'handle_retired', 'handle_unpublished',
    'proof_verifier_configured', 'ceremony_bound', 'bind_fee_paid',
    'deposited', 'deposited_to_handle', 'forwarded', 'claimed', 'refunded'
));

-- Bindings are keyed by node alone; the node lookups span chains.
ALTER TABLE names.ids DROP COLUMN IF EXISTS user_id;
CREATE INDEX IF NOT EXISTS ids_node_chain_idx ON names.ids (id_node, chain_id);
ALTER TABLE names.handles DROP COLUMN IF EXISTS handle;
CREATE INDEX IF NOT EXISTS handles_node_chain_idx
    ON names.handles (handle_node, chain_id);

-- Mirrors `published`: the node a holder's name hashes to, never its text.
ALTER TABLE names.published DROP COLUMN IF EXISTS handle;
ALTER TABLE names.published ADD COLUMN IF NOT EXISTS handle_node BYTEA NOT NULL;

-- A platform some bind used: the half of the contract's `_requireUsable` an
-- index can see.
ALTER TABLE names.platforms
    DROP COLUMN IF EXISTS configured_count,
    DROP COLUMN IF EXISTS last_configured_block;

-- Handle text the chain disclosed: by a disclosing bind, a `publish`, or a
-- `depositToHandle`. Folded, and checked to hash to its node before it is
-- written. One row per node; the latest disclosure names the source, and a
-- holder's HandleUnpublished marks it withdrawn.
CREATE TABLE IF NOT EXISTS names.disclosures (
    chain_id     BIGINT      NOT NULL,
    handle_node  BYTEA       NOT NULL,
    platform_id  BYTEA       NOT NULL,
    handle       TEXT        NOT NULL,
    source       TEXT        NOT NULL CHECK (source IN ('bind', 'publish', 'escrow')),
    block_number BIGINT      NOT NULL,
    log_index    BIGINT      NOT NULL,
    withdrawn    BOOLEAN     NOT NULL DEFAULT false,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, handle_node)
);
CREATE INDEX IF NOT EXISTS disclosures_node_chain_idx
    ON names.disclosures (handle_node, chain_id);
-- A search on one platform narrows here; the text filters the rows it reads.
CREATE INDEX IF NOT EXISTS disclosures_platform_chain_idx
    ON names.disclosures (platform_id, chain_id);
CREATE INDEX IF NOT EXISTS disclosures_handle_trgm_idx
    ON names.disclosures USING gin (handle gin_trgm_ops);

-- The visibility policy, in one place: the handle text the API may show.
-- Every read that shows text joins this view. A withdrawn disclosure stays
-- hidden until the chain discloses the node again.
CREATE OR REPLACE VIEW names.visible_handles AS
SELECT chain_id, platform_id, handle_node, handle, source
FROM names.disclosures
WHERE NOT withdrawn;

-- Each visible handle and the identity behind it: the id node, never an id.
-- id_node and holder are NULL until a bind takes the node.
CREATE OR REPLACE VIEW names.handle_identities AS
SELECT v.chain_id, v.platform_id, v.handle, v.handle_node, h.id_node,
       h.owner AS holder
FROM names.visible_handles v
LEFT JOIN names.handles h
  ON h.chain_id = v.chain_id AND h.handle_node = v.handle_node;
