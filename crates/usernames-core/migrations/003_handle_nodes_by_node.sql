-- A handle node's platform and text on every chain, for the histories and
-- escrow amounts asked by node.
CREATE INDEX IF NOT EXISTS handle_nodes_node_chain_idx
    ON names.handle_nodes (handle_node, chain_id);
