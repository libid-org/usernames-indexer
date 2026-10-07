-- Mirrors claim(): the slot empties and the next round opens. Returns the
-- slot it moved, so a claim on a slot this index never saw deposited is
-- reported instead of passing silently.
UPDATE names.escrow_held SET
    held = 0,
    round = $4::numeric + 1,
    block_number = $5,
    log_index = $6,
    updated_at = now()
WHERE chain_id = $1 AND handle_node = $2 AND token = $3
RETURNING handle_node
