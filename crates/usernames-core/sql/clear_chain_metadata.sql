-- Everything the chain accumulated goes, by column, so a key added later is
-- cleared by default instead of surviving a replay because a list forgot it.
-- The one carve-out is the deployment-block cache: it is keyed by contract,
-- and eth_getCode history does not change shape with the read model.
DELETE FROM names.chain_metadata
WHERE chain_id = $1 AND key NOT LIKE 'deploy_block:%'
