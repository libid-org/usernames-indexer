-- One `Referred`, by its referrer: the bind it came with, the fee and the
-- share, and for a paid share where the escrow put it. A replayed window
-- never reaches here: the journal's conflict stops it first.
INSERT INTO names.referrals (
    chain_id, block_number, log_index, block_time, referrer_node,
    referrer_platform, binder, platform_id, handle_node, fee, share, outcome,
    delivery
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::numeric, $11::numeric, $12, $13)
