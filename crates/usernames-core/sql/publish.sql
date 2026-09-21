INSERT INTO names.published (chain_id, owner, platform_id, handle)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id, owner, platform_id) DO UPDATE SET
    handle = EXCLUDED.handle,
    updated_at = now()
