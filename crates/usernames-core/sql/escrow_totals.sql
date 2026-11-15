-- An address's unclaimed totals per chain and named token ($2, $3
-- pairwise, and $4, the coin, on every chain): `claimable`, held for nodes
-- it holds on that chain, and `awaiting`, held for its nodes on chains
-- where nobody holds them, as claimable_slots.sql and
-- escrow_awaiting_slots.sql list them. Each node seeks each named key by
-- the escrow's primary key, LIMIT 1 keeping it a seek, so tokens nobody
-- named cost nothing however many a junk deposit wrote.
WITH n AS (
    SELECT DISTINCT handle_node
    FROM names.handles
    WHERE owner = $1
),
k AS (
    SELECT chain_id, $4::bytea AS token FROM names.chains
    UNION
    SELECT chain_id, token FROM unnest($2::bigint[], $3::bytea[]) AS k (chain_id, token)
)
SELECT e.chain_id, e.token,
       COALESCE(SUM(e.held) FILTER (WHERE o.owner = $1), 0)::text AS claimable,
       COALESCE(SUM(e.held) FILTER (WHERE o.owner IS NULL), 0)::text AS awaiting
FROM n
CROSS JOIN k
CROSS JOIN LATERAL (
    SELECT e.chain_id, e.handle_node, e.token, e.held
    FROM names.escrow_held e
    WHERE e.chain_id = k.chain_id AND e.handle_node = n.handle_node
      AND e.token = k.token AND e.held > 0
    LIMIT 1) e
LEFT JOIN names.handles o
  ON o.chain_id = e.chain_id AND o.handle_node = e.handle_node
GROUP BY e.chain_id, e.token
ORDER BY e.chain_id, e.token
