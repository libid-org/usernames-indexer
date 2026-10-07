-- A suite's chain starts from nothing, its deployment-block cache included.
DELETE FROM names.chain_metadata WHERE chain_id = $1
