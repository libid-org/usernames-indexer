-- The target, when it was reported and until when the report is good, in
-- one statement by the database's clock: the three cannot disagree.
UPDATE names.chains SET
    target      = $3,
    reported_at = EXTRACT(EPOCH FROM now())::bigint,
    valid_until = EXTRACT(EPOCH FROM now())::bigint + $4
WHERE chain_id = $1 AND writer = $2
