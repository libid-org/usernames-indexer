SELECT name, chain_id FROM names.chain_names
WHERE name = ANY($1) AND chain_id <> $2
ORDER BY name
