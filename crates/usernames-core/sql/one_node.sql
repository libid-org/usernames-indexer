-- One handle node as `h`, in the shape holder_nodes.sql gives a holder's:
-- the node asked alone, with no platform, since a share can be held under a
-- node no bind filed. The caller binds the node as `handle_node` and closes
-- `h`.
WITH h AS (
    SELECT NULL::bytea AS platform_id,
