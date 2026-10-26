SELECT a.chain_id, a.block_number, a.log_index, a.block_time, a.roles,
       e.tx_hash, e.kind, e.payload,
       he.handle_node, v.platform_id, v.handle
FROM names.address_events a
JOIN names.events e
  ON e.chain_id = a.chain_id AND e.block_number = a.block_number
     AND e.log_index = a.log_index
LEFT JOIN names.handle_events he
  ON he.chain_id = a.chain_id AND he.block_number = a.block_number
     AND he.log_index = a.log_index
LEFT JOIN names.handle_nodes v
  ON v.chain_id = he.chain_id AND v.handle_node = he.handle_node
