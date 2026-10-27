-- A node's platform, from the first event that names it, and its text from
-- the first disclosure: a later one carries the same text, since both hash
-- to the node. A row that already holds both is left as it is.
INSERT INTO names.handle_nodes (chain_id, handle_node, platform_id, handle)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id, handle_node) DO UPDATE SET handle = EXCLUDED.handle
WHERE names.handle_nodes.handle IS NULL AND EXCLUDED.handle IS NOT NULL
