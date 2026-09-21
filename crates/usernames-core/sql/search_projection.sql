SELECT h.chain_id, h.platform_id, h.handle, h.owner, i.user_id,
       (p.handle IS NOT NULL) AS published
FROM names.handles h
LEFT JOIN names.ids i
  ON i.chain_id = h.chain_id AND i.id_node = h.id_node
LEFT JOIN names.published p
  ON p.chain_id = h.chain_id AND p.owner = h.owner
     AND p.platform_id = h.platform_id AND p.handle = h.handle
WHERE h.owner IS NOT NULL
