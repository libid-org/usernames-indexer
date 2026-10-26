SELECT h.chain_id, h.platform_id, h.handle_node, h.owner, h.observed_at,
       h.ceremony_version, h.id_node, v.handle
FROM names.handles h
LEFT JOIN names.handle_nodes v
  ON v.chain_id = h.chain_id AND v.handle_node = h.handle_node
