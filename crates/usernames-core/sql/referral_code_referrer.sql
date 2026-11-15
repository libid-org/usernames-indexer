-- A referrer's handle node on every chain that bound it: its platform, its
-- holder there, and its text where that holder published it as its name.
SELECT h.chain_id, h.platform_id, h.owner,
       CASE WHEN p.owner IS NOT NULL THEN v.handle END AS published_handle
FROM names.handles h
LEFT JOIN names.published p
  ON p.chain_id = h.chain_id AND p.owner = h.owner
 AND p.handle_node = h.handle_node
LEFT JOIN names.handle_nodes v
  ON v.chain_id = h.chain_id AND v.handle_node = h.handle_node
WHERE h.handle_node = $1
ORDER BY h.chain_id
