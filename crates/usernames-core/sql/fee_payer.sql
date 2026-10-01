-- Who paid a bind fee, and for which handle: the CeremonyBound of the same
-- bind carries the same digest in the same transaction, names the holder
-- whose value paid the fee, and already has its handle recorded.
SELECT c.payload->>'holder' AS holder, he.handle_node, he.platform_id
FROM names.events c
LEFT JOIN names.handle_events he
  ON he.chain_id = c.chain_id AND he.block_number = c.block_number
     AND he.log_index = c.log_index
WHERE c.chain_id = $1 AND c.block_number = $2 AND c.tx_hash = $3
  AND c.kind = 'ceremony_bound' AND c.payload->>'authorizationDigest' = $4
