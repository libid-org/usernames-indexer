SELECT h.chain_id, h.handle, h.handle_node, h.owner, h.observed_at,
       h.ceremony_version, h.id_node, i.user_id
FROM names.handles h
LEFT JOIN names.ids i
  ON i.chain_id = h.chain_id AND i.id_node = h.id_node
