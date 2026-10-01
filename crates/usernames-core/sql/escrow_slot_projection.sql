SELECT e.chain_id, e.platform_id, e.handle_node, h.handle, h.owner AS holder,
       e.token, e.held::text AS amount, e.round::text AS round
FROM names.escrow_held e
LEFT JOIN names.handles h
  ON h.chain_id = e.chain_id AND h.handle_node = e.handle_node
WHERE e.held > 0
