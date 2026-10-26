-- Mirrors deposit() into a node nobody holds: the slot holds more, under the
-- round the deposit names.
INSERT INTO names.escrow_held (chain_id, handle_node, token, held, round)
VALUES ($1, $2, $3, $4::numeric, $5::numeric)
ON CONFLICT (chain_id, handle_node, token) DO UPDATE SET
    held = names.escrow_held.held + EXCLUDED.held,
    round = EXCLUDED.round
