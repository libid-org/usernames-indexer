-- Reads answer across every chain in the store since the API stopped taking
-- a chain, and the indexes led with chain_id, which left a lookup without
-- one no path but a scan. These lead with what a read asks by and end with
-- chain_id: the same index serves the read scoped to one chain, and yields
-- the per-chain order the API returns. Like 001, re-appliable.

DROP INDEX IF EXISTS names.handles_owner_idx;
DROP INDEX IF EXISTS names.handles_platform_handle_idx;
DROP INDEX IF EXISTS names.ids_owner_idx;
DROP INDEX IF EXISTS names.ids_platform_user_idx;

-- Equality on (platform, handle), and prefix search through text_pattern_ops.
CREATE INDEX IF NOT EXISTS handles_platform_handle_chain_idx
    ON names.handles (platform_id, handle text_pattern_ops, chain_id);
-- The handles a wallet holds; retired rows never match a search.
CREATE INDEX IF NOT EXISTS handles_owner_chain_idx
    ON names.handles (owner, chain_id) WHERE owner IS NOT NULL;
CREATE INDEX IF NOT EXISTS ids_platform_user_chain_idx
    ON names.ids (platform_id, user_id, chain_id);
CREATE INDEX IF NOT EXISTS ids_owner_chain_idx
    ON names.ids (owner, chain_id);
CREATE INDEX IF NOT EXISTS platforms_platform_idx
    ON names.platforms (platform_id);
