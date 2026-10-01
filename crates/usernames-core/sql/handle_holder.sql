-- Who held a handle before a retirement clears it: NULL for a node never
-- bound.
SELECT owner FROM names.handles WHERE chain_id = $1 AND handle_node = $2
