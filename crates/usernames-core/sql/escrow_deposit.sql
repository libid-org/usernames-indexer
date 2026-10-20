-- Mirrors deposit() into a node nobody holds: the slot holds more, under the
-- round the deposit names.
INSERT INTO names.escrow_held
    (chain_id, handle_node, token, held, round, block_number, log_index)
VALUES ($1, $2, $3, $4::numeric, $5::numeric, $6, $7)
ON CONFLICT (chain_id, handle_node, token) DO UPDATE SET
    held = names.escrow_held.held + EXCLUDED.held,
    round = EXCLUDED.round,
    block_number = EXCLUDED.block_number,
    log_index = EXCLUDED.log_index,
    updated_at = now()
