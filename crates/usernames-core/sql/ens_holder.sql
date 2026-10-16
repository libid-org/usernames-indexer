-- The gateway answers every binding, a private one too, as `resolveHandle` does.
SELECT owner FROM names.handles WHERE chain_id = $1 AND handle_node = $2
