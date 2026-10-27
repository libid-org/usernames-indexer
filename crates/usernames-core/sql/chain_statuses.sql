-- Every chain's row and the names it goes by: what /v1/status lists.
SELECT c.chain_id, c.indexer_version, c.contract, c.escrow, c.deploy_block,
       c.cursor, c.head, c.target, c.reported_at,
       c.valid_until - EXTRACT(EPOCH FROM now())::bigint AS valid_for,
       c.window_error, c.proof_verifier,
       COALESCE(n.names, '{}') AS names
FROM names.chains c
LEFT JOIN LATERAL (
    SELECT array_agg(name ORDER BY name) AS names
    FROM names.chain_names
    WHERE chain_id = c.chain_id
) n ON true
ORDER BY c.chain_id
