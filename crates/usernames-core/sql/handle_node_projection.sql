-- What the store knows of a handle node on each chain that named it: its
-- platform, and its text where that chain disclosed it. The caller narrows
-- the node and the chain.
SELECT v.platform_id, v.handle
FROM names.handle_nodes v
