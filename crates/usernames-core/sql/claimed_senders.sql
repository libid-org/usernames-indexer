-- Whose deposits a claim takes: every sender still booked in the slot. A
-- refund already deleted its own row.
SELECT sender FROM names.escrow_refundable
WHERE handle_node = $2 AND chain_id = $1 AND token = $3
