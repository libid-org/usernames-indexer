-- The nodes of the handles a holder holds, each once, from the first chain
-- it holds the node on: `h`, which a holder's reads go through node by node.
-- The caller names the holder, may narrow the nodes, then closes `h` with
-- `ORDER BY handle_node, chain_id)`, which picks that first chain.
WITH h AS (
    SELECT DISTINCT ON (handle_node) chain_id, platform_id, handle_node
    FROM names.handles
    WHERE owner =
