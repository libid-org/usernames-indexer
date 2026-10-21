-- Mirrors the contract's delete of the holder's name, and returns the node
-- the name hashed to.
DELETE FROM names.published
WHERE chain_id = $1 AND owner = $2
RETURNING handle_node
