INSERT INTO names.handles
    (chain_id, handle_node, platform_id, owner,
     observed_at, ceremony_version, id_node)
VALUES ($1, $2, $3, $4, $5, $6, $7)
ON CONFLICT (chain_id, handle_node) DO UPDATE SET
    owner = EXCLUDED.owner,
    observed_at = EXCLUDED.observed_at,
    ceremony_version = EXCLUDED.ceremony_version,
    id_node = EXCLUDED.id_node
