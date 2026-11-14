-- Claims that brought their holder in: the claimed round took a deposit of
-- the token while nobody held the node, and the node was bound after that
-- deposit and before the claim. A claim whose deposits came before the
-- index's first block is not one. The binds are tested against the round's
-- first deposit alone, so a handle's deposits and binds cost their sum, not
-- their product. Per chain and token: once over the whole journal (day
-- NULL), and once per UTC day. Tokens are named or folded as in
-- stats_escrow.sql: the chain's coin ($1) and the priced (chain, token)
-- pairs ($2, $3) by name, every other one in a row with no token and no
-- amount.
WITH named (chain_id, token) AS (
    SELECT * FROM unnest($2::bigint[], $3::bytea[])
)
SELECT chain_id, day, token,
       count(*) AS count,
       CASE WHEN token IS NOT NULL THEN sum(released)::text END AS amount,
       count(DISTINCT claimer) AS wallets
FROM (
    SELECT c.chain_id,
           CASE WHEN c.token = $1
                  OR (c.chain_id, c.token) IN (SELECT chain_id, token FROM named)
                THEN c.token END AS token,
           c.released, c.wallet AS claimer, c.day
    FROM names.events c
    WHERE c.kind = 'claimed'
      AND EXISTS (
          SELECT 1
          FROM names.events b
          WHERE b.handle_node = c.handle_node
            AND b.chain_id = c.chain_id
            AND b.kind = 'identity_bound'
            AND (b.block_number, b.log_index) < (c.block_number, c.log_index)
            AND (b.block_number, b.log_index) > (
                SELECT d.block_number, d.log_index
                FROM names.events d
                WHERE d.handle_node = c.handle_node
                  AND d.chain_id = c.chain_id
                  AND d.kind = 'deposited'
                  AND d.token = c.token
                  AND d.round = c.round
                ORDER BY d.block_number, d.log_index
                LIMIT 1)
      )
) e
GROUP BY GROUPING SETS ((chain_id, token), (chain_id, day, token))
ORDER BY chain_id, day NULLS FIRST, token NULLS LAST
