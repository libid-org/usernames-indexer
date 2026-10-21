-- Mirrors the contract's overwrite of the holder's one name, whichever
-- platform it was on.
INSERT INTO names.published (chain_id, owner, platform_id, handle_node)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id, owner) DO UPDATE SET
    platform_id = EXCLUDED.platform_id,
    handle_node = EXCLUDED.handle_node,
    updated_at = now()
