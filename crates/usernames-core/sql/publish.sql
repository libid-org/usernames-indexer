-- Mirrors the contract's overwrite of `published`.
INSERT INTO names.published (chain_id, owner, platform_id, handle_node)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id, owner, platform_id) DO UPDATE SET
    handle_node = EXCLUDED.handle_node,
    updated_at = now()
