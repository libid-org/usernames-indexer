-- The deposit's refundTo may take this much more back, until the round closes.
INSERT INTO names.escrow_refundable
    (chain_id, handle_node, token, round, refund_to, amount)
VALUES ($1, $2, $3, $4::numeric, $5, $6::numeric)
ON CONFLICT (refund_to, chain_id, handle_node, token, round) DO UPDATE SET
    amount = names.escrow_refundable.amount + EXCLUDED.amount,
    updated_at = now()
