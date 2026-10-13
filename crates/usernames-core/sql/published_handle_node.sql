-- The node an unpublish withdraws.
SELECT handle_node FROM names.published
WHERE chain_id = $1 AND owner = $2 AND platform_id = $3
