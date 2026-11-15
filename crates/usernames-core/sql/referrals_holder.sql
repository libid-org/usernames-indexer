-- The rest of a referral history, on the page's rows alone: the transaction
-- of each bind, from the journal row under the referral's own key; and each
-- binder's published handle on the referral's chain as it stands now. The
-- caller appends the referrer handle's text as `rv`, then orders the page
-- again.
SELECT r.chain_id, r.block_number, r.log_index, e.tx_hash, r.block_time,
       r.referrer_node, r.referrer_platform, r.binder, r.platform_id,
       r.handle_node, r.fee, r.share, r.outcome, r.delivery,
       rv.handle AS referrer_handle, v.handle AS binder_handle
FROM page r
JOIN names.events e
  ON e.chain_id = r.chain_id AND e.block_number = r.block_number
 AND e.log_index = r.log_index
LEFT JOIN names.published p
  ON p.chain_id = r.chain_id AND p.owner = r.binder
LEFT JOIN names.handle_nodes v
  ON v.chain_id = p.chain_id AND v.handle_node = p.handle_node
