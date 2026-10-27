-- One chain's row, with how long its last report stays good by the
-- database's clock, the one that stamped it.
SELECT chain_id, indexer_version, contract, escrow, deploy_block, cursor, head,
       target, reported_at,
       valid_until - EXTRACT(EPOCH FROM now())::bigint AS valid_for,
       window_error, proof_verifier
FROM names.chains
WHERE chain_id = $1
