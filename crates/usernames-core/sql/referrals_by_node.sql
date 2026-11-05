-- A holder's referrals: the nodes of the handles it holds on any chain,
-- through the owner index, and each node's referrals, a page's worth at most,
-- newest first. The caller narrows the referrals past a page's cursor and
-- caps them, then appends the rest. Read per node, as the claimable slots
-- are: Postgres estimates a row comparison led by block time as matching
-- almost nothing, and would walk the whole table past a cursor.
SELECT r.chain_id, r.block_number, r.log_index, r.block_time, r.referrer_node,
       r.referrer_platform, r.binder, r.platform_id, r.handle_node, r.fee,
       r.share, r.outcome, r.delivery, rv.handle AS referrer_handle,
       v.handle AS binder_handle
FROM names.handles h
CROSS JOIN LATERAL (
    SELECT r.chain_id, r.block_number, r.log_index, r.block_time,
           r.referrer_node, r.referrer_platform, r.binder, r.platform_id,
           r.handle_node, r.fee::text AS fee, r.share::text AS share,
           r.outcome, r.delivery
    FROM names.referrals r
    WHERE r.referrer_node = h.handle_node
