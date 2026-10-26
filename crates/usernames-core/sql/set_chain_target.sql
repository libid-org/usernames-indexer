-- The target, when it was reported and until when the report is good, in
-- one statement by the database's clock: the three cannot disagree.
INSERT INTO names.chains (chain_id, target, reported_at, valid_until)
VALUES ($1, $2, EXTRACT(EPOCH FROM now())::bigint,
        EXTRACT(EPOCH FROM now())::bigint + $3)
ON CONFLICT (chain_id) DO UPDATE SET
    target      = EXCLUDED.target,
    reported_at = EXCLUDED.reported_at,
    valid_until = EXCLUDED.valid_until
