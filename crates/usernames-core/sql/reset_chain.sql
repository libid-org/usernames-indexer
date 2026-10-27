-- A replay from the deployment block: the chain row starts over under a new
-- writer, the version and contracts now written, and the deployment block
-- it carries.
INSERT INTO names.chains (chain_id, writer, indexer_version, contract, escrow, deploy_block)
VALUES ($1, nextval('names.chain_writers'), $2, $3, $4, $5)
RETURNING writer
