-- The text any chain disclosed for the handle node in {node}, as
-- {alias}.handle, read from the lowest chain id that disclosed it: a node
-- hashes its platform and text, so every chain's text is the same.
LEFT JOIN LATERAL (
    SELECT t.handle
    FROM names.handle_nodes t
    WHERE t.handle_node = {node} AND t.handle IS NOT NULL
    ORDER BY t.chain_id
    LIMIT 1
) {alias} ON true
