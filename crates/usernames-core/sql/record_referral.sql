-- One `Referred`, by its referrer: the bind it came with, the fee and the
-- share, and for a paid share its token, where the escrow put it and, when
-- held, the slot round that settles it. A replayed window never reaches here:
-- the journal's conflict stops it first.
INSERT INTO names.referrals (
    chain_id, block_number, log_index, block_time, referrer_node, binder,
    platform_id, handle_node, fee, share, outcome, delivery, token, round
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::numeric, $10::numeric, $11, $12, $13,
        $14::numeric)
