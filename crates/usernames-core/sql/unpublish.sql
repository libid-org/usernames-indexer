DELETE FROM names.published
WHERE chain_id = $1 AND owner = $2 AND platform_id = $3
