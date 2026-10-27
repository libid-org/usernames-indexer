-- The node a holder's name hashes to, which an unpublish withdraws.
SELECT handle_node FROM names.published
WHERE chain_id = $1 AND owner = $2
