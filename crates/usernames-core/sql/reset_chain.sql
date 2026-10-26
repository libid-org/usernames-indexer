-- A replay from the deployment block: the chain row starts over under the
-- version and contracts now written, keeping the deployment block only
-- while the registry stays the same.
INSERT INTO names.chains (chain_id, indexer_version, contract, escrow)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id) DO UPDATE SET
    indexer_version = EXCLUDED.indexer_version,
    contract        = EXCLUDED.contract,
    escrow          = EXCLUDED.escrow,
    deploy_block    = CASE WHEN names.chains.contract = EXCLUDED.contract
                           THEN names.chains.deploy_block END,
    cursor          = NULL,
    head            = NULL,
    target          = NULL,
    reported_at     = NULL,
    valid_until     = NULL,
    window_error    = NULL,
    proof_verifier  = NULL
