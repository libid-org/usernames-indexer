SELECT he.chain_id, he.block_number, he.log_index, he.block_time,
       NULL::text[] AS roles,
       e.tx_hash, e.kind, e.payload,
       he.handle_node, hn.platform_id, v.handle
FROM names.handle_events he
JOIN names.events e
  ON e.chain_id = he.chain_id AND e.block_number = he.block_number
     AND e.log_index = he.log_index
LEFT JOIN names.handle_nodes hn
  ON hn.chain_id = he.chain_id AND hn.handle_node = he.handle_node
LEFT JOIN names.visible_handles v
  ON v.chain_id = he.chain_id AND v.handle_node = he.handle_node
