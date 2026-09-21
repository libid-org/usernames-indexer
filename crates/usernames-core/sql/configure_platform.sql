INSERT INTO names.platforms
    (chain_id, platform_id, platform_key, configured_count, last_configured_block)
VALUES ($1, $2, $3, 1, $4)
ON CONFLICT (chain_id, platform_id) DO UPDATE SET
    platform_key = EXCLUDED.platform_key,
    configured_count = names.platforms.configured_count + 1,
    last_configured_block = EXCLUDED.last_configured_block,
    updated_at = now()
RETURNING configured_count > 1
