-- The ceremony a bind fee was paid beside, whose holder paid it: the
-- CeremonyBound of the same bind carries the same digest in the same
-- transaction. Its journal row, and the handle its bind recorded.
SELECT c.kind, c.payload, he.handle_node
FROM names.events c
LEFT JOIN names.handle_events he
  ON he.chain_id = c.chain_id AND he.block_number = c.block_number
     AND he.log_index = c.log_index
WHERE c.chain_id = $1 AND c.block_number = $2 AND c.tx_hash = $3
  AND c.kind = 'ceremony_bound' AND c.payload->>'authorizationDigest' = $4
