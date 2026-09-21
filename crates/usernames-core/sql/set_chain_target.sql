INSERT INTO names.chain_metadata (chain_id, key, value)
VALUES ($1, $2, $3),
       ($1, $4, EXTRACT(EPOCH FROM now())::bigint::text),
       ($1, $5, (EXTRACT(EPOCH FROM now())::bigint + $6)::text)
ON CONFLICT (chain_id, key) DO UPDATE SET value = EXCLUDED.value
