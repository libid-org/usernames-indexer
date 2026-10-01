-- A claim ends every refund booked in the round it closed.
DELETE FROM names.escrow_refundable
WHERE chain_id = $1 AND handle_node = $2 AND token = $3 AND round = $4::numeric
