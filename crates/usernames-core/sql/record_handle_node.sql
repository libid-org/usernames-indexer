-- A node's platform, from the first event that names it, and its text from
-- the first disclosure: a later one carries the same text, since both hash
-- to the node.
INSERT INTO names.handle_nodes (chain_id, handle_node, platform_id, handle)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id, handle_node) DO UPDATE SET
    handle = COALESCE(names.handle_nodes.handle, EXCLUDED.handle)
