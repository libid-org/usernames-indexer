-- Mirrors refund(): the slot holds the sender's contribution less, as the
-- index booked it, so the slot always holds what its senders could take
-- back. Returns the slot it moved.
UPDATE names.escrow_held SET held = held - $4::numeric
WHERE chain_id = $1 AND handle_node = $2 AND token = $3
RETURNING handle_node
