-- What waits for a holder on chains where nobody holds its handles, after
-- `h` (holder_nodes.sql): each node's slots on every chain where no address
-- holds that node, a page's worth at most. Binding the handle there makes
-- each slot claimable. The caller narrows the slots past a page's cursor
-- and caps them, then appends the rest.
SELECT e.chain_id, h.platform_id, e.handle_node, v.handle, NULL::bytea AS holder,
       e.token, e.held::text AS amount, e.round::text AS round
FROM h
CROSS JOIN LATERAL (
    SELECT e.chain_id, e.handle_node, e.token, e.held, e.round
    FROM names.escrow_held e
    WHERE e.handle_node = h.handle_node AND e.held > 0
      AND NOT EXISTS (
          SELECT 1
          FROM names.handles o
          WHERE o.chain_id = e.chain_id AND o.handle_node = e.handle_node
            AND o.owner IS NOT NULL
      )
