SELECT e.chain_id, e.platform_id, e.handle_node, h.handle, h.owner AS holder,
       e.token, e.held::text AS amount, e.round::text AS round
FROM names.handles h
JOIN names.escrow_held e
  ON e.chain_id = h.chain_id AND e.handle_node = h.handle_node
WHERE e.held > 0
