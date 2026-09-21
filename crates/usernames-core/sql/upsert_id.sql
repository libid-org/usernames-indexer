INSERT INTO names.ids
    (chain_id, id_node, platform_id, user_id, owner,
     observed_at, ceremony_version, handle_node, block_number, log_index)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
ON CONFLICT (chain_id, id_node) DO UPDATE SET
    owner = EXCLUDED.owner,
    observed_at = EXCLUDED.observed_at,
    ceremony_version = EXCLUDED.ceremony_version,
    handle_node = EXCLUDED.handle_node,
    block_number = EXCLUDED.block_number,
    log_index = EXCLUDED.log_index,
    updated_at = now()
