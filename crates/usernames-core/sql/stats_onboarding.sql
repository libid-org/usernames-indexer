-- Claims that brought their holder in: the claimed round took a deposit of
-- the token while nobody held the node, and the node was bound after that
-- deposit and before the claim. A claim whose deposits came before the
-- index's first block is not one. Per chain and token: once over the whole
-- journal (day NULL), and once per UTC day.
SELECT chain_id, day, token,
       count(*) AS count,
       sum(released)::text AS amount,
       count(DISTINCT claimer) AS wallets
FROM (
    SELECT c.chain_id, c.token, c.released, c.claimer,
           c.block_time - c.block_time % 86400 AS day
    FROM names.events c
    WHERE c.kind = 'claimed'
      AND EXISTS (
          SELECT 1
          FROM names.events d
          JOIN names.events b
            ON b.handle_node = d.handle_node
           AND b.chain_id = d.chain_id
           AND b.kind = 'identity_bound'
           AND (b.block_number, b.log_index) > (d.block_number, d.log_index)
          WHERE d.handle_node = c.handle_node
            AND d.chain_id = c.chain_id
            AND d.kind = 'deposited'
            AND d.token = c.token
            AND d.round = c.round
            AND (b.block_number, b.log_index) < (c.block_number, c.log_index)
      )
) e
GROUP BY GROUPING SETS ((chain_id, token), (chain_id, day, token))
ORDER BY chain_id, day NULLS FIRST, token
