-- The text of the handle node in {node}, as {alias}.handle, from the first
-- chain that disclosed it: a node hashes its platform and text, so the text
-- one chain disclosed is the text on every chain.
LEFT JOIN LATERAL (
    SELECT t.handle
    FROM names.handle_nodes t
    WHERE t.handle_node = {node} AND t.handle IS NOT NULL
    ORDER BY t.chain_id
    LIMIT 1
) {alias} ON true
