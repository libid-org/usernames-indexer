SELECT h.chain_id, h.platform_id, h.handle_node, h.id_node, v.handle, h.owner,
       (p.handle_node IS NOT NULL) AS published
FROM names.handle_nodes v
JOIN names.handles h
  ON h.chain_id = v.chain_id AND h.handle_node = v.handle_node
LEFT JOIN names.published p
  ON p.chain_id = h.chain_id AND p.owner = h.owner
     AND p.platform_id = h.platform_id AND p.handle_node = h.handle_node
WHERE h.owner IS NOT NULL AND v.handle IS NOT NULL
