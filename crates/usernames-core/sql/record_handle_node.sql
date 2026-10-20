-- A node's platform, from the first event that names it.
INSERT INTO names.handle_nodes (chain_id, handle_node, platform_id)
VALUES ($1, $2, $3)
ON CONFLICT (chain_id, handle_node) DO NOTHING
