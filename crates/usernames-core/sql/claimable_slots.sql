-- What a holder may claim: its handles, through the owner index, and each
-- one's slots looked up by node, a page's worth at most. The caller narrows
-- the slots past a page's cursor and caps them, then narrows the handles.
-- Postgres estimates a row comparison by its leading column alone, which
-- makes a walk of every slot past a cursor look free; read per handle, the
-- slots offer no such walk.
SELECT e.chain_id, h.platform_id, e.handle_node, v.handle, h.owner AS holder,
       e.token, e.held::text AS amount, e.round::text AS round
FROM names.handles h
CROSS JOIN LATERAL (
    SELECT e.chain_id, e.handle_node, e.token, e.held, e.round
    FROM names.escrow_held e
    WHERE e.chain_id = h.chain_id AND e.handle_node = h.handle_node
      AND e.held > 0
