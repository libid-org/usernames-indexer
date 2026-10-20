SELECT r.chain_id, n.platform_id, r.handle_node, h.handle, h.owner AS holder,
       r.token, r.amount::text AS amount, r.round::text AS round
FROM names.escrow_refundable r
JOIN names.handle_nodes n
  ON n.chain_id = r.chain_id AND n.handle_node = r.handle_node
LEFT JOIN names.handles h
  ON h.chain_id = r.chain_id AND h.handle_node = r.handle_node
