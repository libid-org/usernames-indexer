-- A replay from the deployment block: the chain row starts over under the
-- version and contracts now written, with the deployment block it carries.
INSERT INTO names.chains (chain_id, indexer_version, contract, escrow, deploy_block)
VALUES ($1, $2, $3, $4, $5)
