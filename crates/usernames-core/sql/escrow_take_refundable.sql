-- A refund pays the whole contribution, so the contract zeroes it. Returns
-- what the index had booked for the sender.
DELETE FROM names.escrow_refundable
WHERE chain_id = $1 AND handle_node = $2 AND token = $3 AND sender = $4
RETURNING amount::text
