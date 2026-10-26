-- Mirrors refund(): the slot holds what the refund released less. Returns
-- the slot it moved, for the same report as a claim.
UPDATE names.escrow_held SET held = held - $4::numeric
WHERE chain_id = $1 AND handle_node = $2 AND token = $3
RETURNING handle_node
