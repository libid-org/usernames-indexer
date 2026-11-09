-- The rest of a referral history: the referrer handle's disclosed text on
-- the chain `h` takes its node from, and each binder's published handle on
-- the referral's chain as it stands now. The caller orders and caps the
-- page.
LEFT JOIN names.handle_nodes rv
  ON rv.chain_id = h.chain_id AND rv.handle_node = h.handle_node
LEFT JOIN names.published p
  ON p.chain_id = r.chain_id AND p.owner = r.binder
LEFT JOIN names.handle_nodes v
  ON v.chain_id = p.chain_id AND v.handle_node = p.handle_node
