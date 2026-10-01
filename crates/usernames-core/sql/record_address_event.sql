INSERT INTO names.address_events
    (chain_id, address, block_number, log_index, block_time, roles)
VALUES ($1, $2, $3, $4, $5, $6)
ON CONFLICT (chain_id, address, block_number, log_index) DO NOTHING
