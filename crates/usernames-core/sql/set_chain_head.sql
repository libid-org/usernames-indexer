INSERT INTO names.chains (chain_id, head) VALUES ($1, $2)
ON CONFLICT (chain_id) DO UPDATE SET head = EXCLUDED.head
