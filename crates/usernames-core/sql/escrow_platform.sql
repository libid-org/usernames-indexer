-- The platform of an escrowed node, as its first deposit named it. Every
-- token's slot carries the same one.
SELECT platform_id FROM names.escrow_held
WHERE chain_id = $1 AND handle_node = $2
LIMIT 1
