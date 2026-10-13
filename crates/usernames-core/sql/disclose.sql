-- Text a later disclosure carries is the same text: both hash to the node.
INSERT INTO names.disclosures (chain_id, handle_node, platform_id, handle)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id, handle_node) DO UPDATE SET withdrawn = false
