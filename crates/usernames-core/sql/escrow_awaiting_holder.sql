-- The rest of an awaiting read: each slot's disclosed text, and the holder
-- whose handles are read, each node once, from the first chain the holder
-- holds it on. The caller names the holder, then orders and caps the page.
LEFT JOIN names.handle_nodes v
  ON v.chain_id = e.chain_id AND v.handle_node = e.handle_node
WHERE NOT EXISTS (
    SELECT 1
    FROM names.handles o
    WHERE o.owner = h.owner AND o.chain_id < h.chain_id
      AND o.handle_node = h.handle_node
) AND h.owner =
