-- The deposit's sender may take this much more back, until the round closes.
-- Each deposit moves the date `refund` opens for the sender's whole
-- contribution.
INSERT INTO names.escrow_refundable
    (chain_id, handle_node, token, sender, amount, refundable_at)
VALUES ($1, $2, $3, $4, $5::numeric, $6)
ON CONFLICT (sender, chain_id, handle_node, token) DO UPDATE SET
    amount = names.escrow_refundable.amount + EXCLUDED.amount,
    refundable_at = EXCLUDED.refundable_at,
    updated_at = now()
