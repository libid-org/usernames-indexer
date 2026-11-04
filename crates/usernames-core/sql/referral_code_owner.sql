-- Bound handle nodes, each with its first bind on any chain, for the node a
-- referral code names: of the nodes its bytes start, the one bound first, by
-- block time, then chain, block and log. A retired node keeps its codes. The
-- caller bounds the nodes by the code's prefix, orders by the first bind and
-- takes one.
SELECT h.handle_node
FROM names.handles h
CROSS JOIN LATERAL (
    SELECT e.block_time, e.chain_id, e.block_number, e.log_index
    FROM names.events e
    WHERE e.handle_node = h.handle_node AND e.kind = 'identity_bound'
    ORDER BY e.block_time, e.chain_id, e.block_number, e.log_index
    LIMIT 1
) bound
