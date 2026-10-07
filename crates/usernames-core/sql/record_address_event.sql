INSERT INTO names.address_events
    (chain_id, address, block_time, block_number, log_index, roles)
VALUES ($1, $2, $3, $4, $5, $6)
ON CONFLICT (chain_id, address, block_time, block_number, log_index) DO NOTHING
