-- The report renewed without moving the target.
UPDATE names.chains SET
    reported_at = EXTRACT(EPOCH FROM now())::bigint,
    valid_until = EXTRACT(EPOCH FROM now())::bigint + $3
WHERE chain_id = $1 AND writer = $2
