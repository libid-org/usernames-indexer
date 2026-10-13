-- Text a later disclosure carries is the same text: both hash to the node.
INSERT INTO names.disclosures
    (chain_id, handle_node, platform_id, handle, source, block_number, log_index)
VALUES ($1, $2, $3, $4, $5, $6, $7)
ON CONFLICT (chain_id, handle_node) DO UPDATE SET
    source = EXCLUDED.source,
    block_number = EXCLUDED.block_number,
    log_index = EXCLUDED.log_index,
    withdrawn = false,
    updated_at = now()
