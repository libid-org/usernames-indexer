SELECT r.chain_id, e.platform_id, r.handle_node, h.handle, h.owner AS holder,
       r.token, r.amount::text AS amount, r.round::text AS round
FROM names.escrow_refundable r
JOIN names.escrow_held e
  ON e.chain_id = r.chain_id AND e.handle_node = r.handle_node
     AND e.token = r.token
LEFT JOIN names.handles h
  ON h.chain_id = r.chain_id AND h.handle_node = r.handle_node
