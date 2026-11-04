-- The tokens /v1/stats asks prices for, each with the sum of its deposits
-- and forwards: per chain, the chain's coin ($1) and the (chain, token)
-- pairs priced before ($2, $3), then the others with the most deposits and
-- forwards, at most $4 of those per chain, ties by token. Anybody can
-- write a token, so a token priced before keeps its place whatever others
-- are moved. Reads every deposit and forward: no index narrows two kinds
-- of four, and it runs once per computation.
WITH held (chain_id, token) AS (
    SELECT * FROM unnest($2::bigint[], $3::bytea[])
),
moved AS (
    SELECT chain_id, token, count(*) AS movements, sum(amount) AS moved
    FROM names.events
    WHERE kind IN ('deposited', 'forwarded')
    GROUP BY chain_id, token
),
ranked AS (
    SELECT chain_id, token,
           token = $1 OR (chain_id, token) IN (SELECT chain_id, token FROM held)
               AS kept,
           movements, moved
    FROM moved
)
SELECT chain_id, token, moved::text AS moved
FROM (
    SELECT chain_id, token, kept, moved,
           row_number() OVER (PARTITION BY chain_id, kept
                              ORDER BY movements DESC, token) AS place
    FROM ranked
) r
WHERE kept OR place <= $4
ORDER BY chain_id, token
