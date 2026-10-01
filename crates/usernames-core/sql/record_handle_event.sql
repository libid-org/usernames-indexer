INSERT INTO names.handle_events
    (chain_id, block_number, log_index, handle_node, platform_id, block_time)
VALUES ($1, $2, $3, $4, $5, $6)
ON CONFLICT (chain_id, block_number, log_index) DO NOTHING
