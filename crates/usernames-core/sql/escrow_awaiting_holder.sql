-- The rest of an awaiting read: each slot's disclosed text. The caller
-- orders and caps the page.
LEFT JOIN names.handle_nodes v
  ON v.chain_id = e.chain_id AND v.handle_node = e.handle_node
