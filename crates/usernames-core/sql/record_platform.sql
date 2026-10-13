INSERT INTO names.platforms (chain_id, platform_id, platform_key)
VALUES ($1, $2, $3)
ON CONFLICT (chain_id, platform_id) DO NOTHING
