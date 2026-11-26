-- The referrals of the nodes in `h` (holder_nodes.sql or one_node.sql):
-- `page`, each node's referrals, a page's worth at most, newest first. Read
-- per node, as the claimable slots are: Postgres estimates a row comparison
-- led by block time as matching almost nothing, and would walk the whole
-- table past a cursor. The caller narrows each node's referrals past a
-- page's cursor and caps them, then orders and caps the page across nodes
-- and closes `page`, so the rest joins onto the page's rows alone. The
-- referrer's platform is the one `h` names for its node, if any.
, page AS (
    SELECT r.*, h.platform_id AS referrer_platform
    FROM h
    CROSS JOIN LATERAL (
        SELECT r.chain_id, r.block_number, r.log_index, r.block_time,
               r.referrer_node, r.binder, r.platform_id,
               r.handle_node, r.fee::text AS fee, r.share::text AS share,
               r.outcome, r.delivery
        FROM names.referrals r
        WHERE r.referrer_node = h.handle_node
