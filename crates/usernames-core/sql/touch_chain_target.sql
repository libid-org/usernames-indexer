-- The report renewed without moving the target.
INSERT INTO names.chains (chain_id, reported_at, valid_until)
VALUES ($1, EXTRACT(EPOCH FROM now())::bigint,
        EXTRACT(EPOCH FROM now())::bigint + $2)
ON CONFLICT (chain_id) DO UPDATE SET
    reported_at = EXCLUDED.reported_at,
    valid_until = EXCLUDED.valid_until
