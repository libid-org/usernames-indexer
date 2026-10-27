SELECT r.chain_id, v.platform_id, r.handle_node, v.handle, h.owner AS holder,
       r.token, r.amount::text AS amount, e.round::text AS round,
       r.refundable_at
FROM names.escrow_refundable r
JOIN names.escrow_held e
  ON e.chain_id = r.chain_id AND e.handle_node = r.handle_node
     AND e.token = r.token
LEFT JOIN names.handle_nodes v
  ON v.chain_id = r.chain_id AND v.handle_node = r.handle_node
LEFT JOIN names.handles h
  ON h.chain_id = r.chain_id AND h.handle_node = r.handle_node
