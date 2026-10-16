-- Mirrors the contract's delete of `published`, and returns the node the
-- name hashed to.
DELETE FROM names.published
WHERE chain_id = $1 AND owner = $2 AND platform_id = $3
RETURNING handle_node
