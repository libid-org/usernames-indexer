-- The tokens /v1/stats asks prices for, each with the sum of its deposits
-- and forwards, from the rollups' totals: per chain, the chain's coin ($1),
-- moved or not, since fees are paid in it, and the (chain, token) pairs
-- priced before ($2, $3), then the others with the most deposits and
-- forwards, at most $4 of those per chain, ties by token. Anybody can write
-- a token, so a token priced before keeps its place whatever others are
-- moved. One seek of the primary key per chain and flow, reading every
-- token the chain's escrow saw and no other key: each chain and flow is its
-- own lateral seek, and OFFSET 0 keeps the planner from flattening it into
-- a join that reads the whole table, so both columns lead the index
-- condition whatever it knows of `names.chains`, which is too small for
-- autovacuum to analyze.
WITH held (chain_id, address) AS (
    SELECT * FROM unnest($2::bigint[], $3::bytea[])
),
moved AS (
    SELECT chain_id, address, sum(count) AS movements, sum(amount) AS moved
    FROM (
        SELECT c.chain_id, t.key AS address, t.count, t.amount
        FROM unnest(ARRAY(SELECT chain_id FROM names.chains)) AS c (chain_id)
        CROSS JOIN unnest(ARRAY['deposited', 'forwarded']) AS f (measure)
        CROSS JOIN LATERAL (
            SELECT t.key, t.count, t.amount
            FROM names.stats_totals t
            WHERE t.chain_id = c.chain_id AND t.measure = f.measure
              AND t.key <> ''
            OFFSET 0) t
        UNION ALL
        SELECT chain_id, $1::bytea, 0, 0
        FROM names.chains
    ) t
    GROUP BY chain_id, address
),
ranked AS (
    SELECT chain_id, address,
           address = $1
               OR (chain_id, address) IN (SELECT chain_id, address FROM held)
               AS kept,
           movements, moved
    FROM moved
)
SELECT chain_id, address, moved::text AS moved
FROM (
    SELECT chain_id, address, kept, moved,
           row_number() OVER (PARTITION BY chain_id, kept
                              ORDER BY movements DESC, address) AS place
    FROM ranked
) r
WHERE kept OR place <= $4
ORDER BY chain_id, address
