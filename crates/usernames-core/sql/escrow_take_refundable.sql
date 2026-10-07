-- A refund pays the whole contribution, so the contract zeroes it.
DELETE FROM names.escrow_refundable
WHERE chain_id = $1 AND handle_node = $2 AND token = $3 AND round = $4::numeric
  AND refund_to = $5
