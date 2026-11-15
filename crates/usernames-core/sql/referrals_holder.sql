-- The rest of a referral history, after the referrer handle's text: the
-- transaction of each bind, from the journal row under the referral's own
-- key; and each binder's published handle on the referral's chain as it
-- stands now. The caller orders and caps the page.
JOIN names.events e
  ON e.chain_id = r.chain_id AND e.block_number = r.block_number
 AND e.log_index = r.log_index
LEFT JOIN names.published p
  ON p.chain_id = r.chain_id AND p.owner = r.binder
LEFT JOIN names.handle_nodes v
  ON v.chain_id = p.chain_id AND v.handle_node = p.handle_node
