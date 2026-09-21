INSERT INTO names.handles
    (chain_id, handle_node, platform_id, handle, owner,
     observed_at, ceremony_version, id_node, block_number, log_index)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
ON CONFLICT (chain_id, handle_node) DO UPDATE SET
    handle = EXCLUDED.handle,
    owner = EXCLUDED.owner,
    observed_at = EXCLUDED.observed_at,
    ceremony_version = EXCLUDED.ceremony_version,
    id_node = EXCLUDED.id_node,
    block_number = EXCLUDED.block_number,
    log_index = EXCLUDED.log_index,
    updated_at = now()
