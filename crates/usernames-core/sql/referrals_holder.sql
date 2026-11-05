-- The rest of a referral history: the referrer handle's disclosed text, each
-- binder's published handle on the referral's chain as it stands now, and
-- the holder whose handles are read, each node once, from the first chain
-- the holder holds it on. The caller names the holder, then orders and caps
-- the page.
LEFT JOIN names.handle_nodes rv
  ON rv.chain_id = h.chain_id AND rv.handle_node = h.handle_node
LEFT JOIN names.published p
  ON p.chain_id = r.chain_id AND p.owner = r.binder
LEFT JOIN names.handle_nodes v
  ON v.chain_id = p.chain_id AND v.handle_node = p.handle_node
WHERE NOT EXISTS (
    SELECT 1
    FROM names.handles o
    WHERE o.owner = h.owner AND o.chain_id < h.chain_id
      AND o.handle_node = h.handle_node
) AND h.owner =
