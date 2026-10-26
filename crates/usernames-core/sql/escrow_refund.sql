-- Mirrors refund(): the slot holds what the refund released less. Returns
-- whether the slot held that much: an index that started after the deposit
-- never saw it, so the slot stops at zero rather than going negative. No row:
-- a slot this index never saw.
WITH slot AS (
    SELECT held FROM names.escrow_held
    WHERE chain_id = $1 AND handle_node = $2 AND token = $3
    FOR UPDATE
)
UPDATE names.escrow_held e SET held = GREATEST(e.held - $4::numeric, 0)
FROM slot
WHERE e.chain_id = $1 AND e.handle_node = $2 AND e.token = $3
RETURNING slot.held >= $4::numeric AS covered
