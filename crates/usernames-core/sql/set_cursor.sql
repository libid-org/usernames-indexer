INSERT INTO names.chains (chain_id, cursor) VALUES ($1, $2)
ON CONFLICT (chain_id) DO UPDATE SET cursor = EXCLUDED.cursor
