-- How many ids a chain binds.
SELECT count(*) FROM names.ids WHERE chain_id = $1
