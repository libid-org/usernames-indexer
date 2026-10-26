-- One decoded log, in the columns its kind carries. A log already journaled
-- inserts nothing, which is what keeps a replayed window from applying twice.
INSERT INTO names.events (
    chain_id, block_number, log_index, tx_hash, block_time, kind, handle_node,
    platform_id, holder, id_node, observed_at, ceremony_version, verifier,
    authorization_digest, client_identifier, receiver, token, sender, claimer,
    recipient, round, amount, released, received, refundable_at
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
        $17, $18, $19, $20, $21::numeric, $22::numeric, $23::numeric,
        $24::numeric, $25)
ON CONFLICT (chain_id, block_number, log_index) DO NOTHING
