INSERT INTO names.events (chain_id, block_number, log_index, tx_hash, kind, payload)
VALUES ($1, $2, $3, $4, $5, $6)
ON CONFLICT (chain_id, block_number, log_index) DO NOTHING
