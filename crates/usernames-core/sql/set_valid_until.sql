INSERT INTO names.chains (chain_id, valid_until) VALUES ($1, $2)
ON CONFLICT (chain_id) DO UPDATE SET valid_until = EXCLUDED.valid_until
