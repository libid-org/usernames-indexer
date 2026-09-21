INSERT INTO names.chain_metadata (chain_id, key, value)
VALUES ($1, $2, $3)
ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value
