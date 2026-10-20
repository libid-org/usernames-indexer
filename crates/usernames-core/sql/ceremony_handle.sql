-- The handle a ceremony proved: the IdentityBound its bind recorded just
-- before it, in the same transaction, for the same holder and platform. The
-- bind sits in the ceremony's block, so it shares the block's time.
SELECT he.handle_node
FROM names.handle_events he
JOIN names.events b
  ON b.chain_id = he.chain_id AND b.block_number = he.block_number
     AND b.log_index = he.log_index
JOIN names.handle_nodes hn
  ON hn.chain_id = he.chain_id AND hn.handle_node = he.handle_node
JOIN names.address_events ae
  ON ae.chain_id = he.chain_id AND ae.block_number = he.block_number
     AND ae.log_index = he.log_index
WHERE he.chain_id = $1 AND he.block_number = $2 AND b.tx_hash = $3
  AND b.kind = 'identity_bound' AND he.log_index < $4
  AND ae.address = $5 AND ae.block_time = $6 AND $7 = ANY(ae.roles)
  AND hn.platform_id = $8
ORDER BY he.log_index DESC
LIMIT 1
