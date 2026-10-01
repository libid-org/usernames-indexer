SELECT a.chain_id, a.block_number, a.log_index, a.block_time, a.roles,
       e.tx_hash, e.kind, e.payload,
       he.handle_node, he.platform_id, h.handle
FROM names.address_events a
JOIN names.events e
  ON e.chain_id = a.chain_id AND e.block_number = a.block_number
     AND e.log_index = a.log_index
LEFT JOIN names.handle_events he
  ON he.chain_id = a.chain_id AND he.block_number = a.block_number
     AND he.log_index = a.log_index
LEFT JOIN names.handles h
  ON h.chain_id = he.chain_id AND h.handle_node = he.handle_node
