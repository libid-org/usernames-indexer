-- Mirrors the contract's delete of the holder's name.
DELETE FROM names.published
WHERE chain_id = $1 AND owner = $2
