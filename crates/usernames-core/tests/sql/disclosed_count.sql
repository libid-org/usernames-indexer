-- How many of a chain's nodes have disclosed text.
SELECT count(*) FROM names.handle_nodes WHERE chain_id = $1 AND handle IS NOT NULL
