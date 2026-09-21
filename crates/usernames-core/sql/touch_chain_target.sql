INSERT INTO names.chain_metadata (chain_id, key, value)
VALUES ($1, $2, EXTRACT(EPOCH FROM now())::bigint::text),
       ($1, $3, (EXTRACT(EPOCH FROM now())::bigint + $4)::text)
ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value
