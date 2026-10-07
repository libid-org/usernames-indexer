-- The handle an unpublish withdraws, as the node the chain keyed it under.
SELECT h.handle_node
FROM names.published p
JOIN names.handles h
  ON h.chain_id = p.chain_id AND h.platform_id = p.platform_id
     AND h.handle = p.handle
WHERE p.chain_id = $1 AND p.owner = $2 AND p.platform_id = $3
