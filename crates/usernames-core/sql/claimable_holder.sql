-- The rest of a claimable read: each slot's disclosed text, and the one
-- holder whose handles are read. The caller narrows the chain and the cursor.
LEFT JOIN names.handle_nodes v
  ON v.chain_id = e.chain_id AND v.handle_node = e.handle_node
WHERE h.owner = 
