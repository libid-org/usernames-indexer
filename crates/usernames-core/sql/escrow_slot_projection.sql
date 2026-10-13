SELECT e.chain_id, n.platform_id, e.handle_node, v.handle, h.owner AS holder,
       e.token, e.held::text AS amount, e.round::text AS round
FROM names.escrow_held e
LEFT JOIN names.handle_nodes n
  ON n.chain_id = e.chain_id AND n.handle_node = e.handle_node
LEFT JOIN names.handles h
  ON h.chain_id = e.chain_id AND h.handle_node = e.handle_node
LEFT JOIN names.visible_handles v
  ON v.chain_id = e.chain_id AND v.handle_node = e.handle_node
WHERE e.held > 0
