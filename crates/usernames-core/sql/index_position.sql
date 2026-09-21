SELECT key, value, EXTRACT(EPOCH FROM now())::bigint
FROM names.chain_metadata
WHERE chain_id = $1 AND key IN ($2, $3, $4, $5)
