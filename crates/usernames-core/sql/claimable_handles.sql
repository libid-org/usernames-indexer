-- What a holder may claim, read from the holder's handles: they are few and
-- the owner index finds them, and each one's slots are then looked up by
-- node. Left to choose, a planner would walk the escrow's slots past a page's
-- cursor instead: it estimates a row comparison by its leading column alone,
-- and a cursor's chain id makes that walk look free. The caller narrows the
-- handles, then reads their slots.
WITH held AS MATERIALIZED (
    SELECT chain_id, handle_node, platform_id, owner
    FROM names.handles
    WHERE owner = 
