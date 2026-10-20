-- Whose deposits a claim takes: every refundTo still booked in the round it
-- closes. A refund already deleted its own row.
SELECT refund_to FROM names.escrow_refundable
WHERE handle_node = $2 AND chain_id = $1 AND token = $3 AND round = $4::numeric
