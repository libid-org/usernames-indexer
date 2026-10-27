-- The text the chain disclosed for a node on one chain, when it did.
SELECT handle FROM names.handle_nodes
WHERE chain_id = $1 AND handle_node = $2 AND handle IS NOT NULL
