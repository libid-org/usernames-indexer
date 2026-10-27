-- How many names an owner publishes on one chain.
SELECT count(*) FROM names.published WHERE chain_id = $1 AND owner = $2
