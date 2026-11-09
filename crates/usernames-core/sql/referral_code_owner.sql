-- The handle nodes any chain has bound, for the nodes a referral code's bytes
-- start. A retired node keeps its codes. The caller bounds the nodes by the
-- code's prefix and takes two: a code names a node only while it is the one
-- bound node those bytes start.
SELECT DISTINCT h.handle_node
FROM names.handles h
WHERE EXISTS (
    SELECT 1
    FROM names.events e
    WHERE e.handle_node = h.handle_node AND e.kind = 'identity_bound'
)
