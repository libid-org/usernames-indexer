-- Mirrors the contract's overwrite of `published`, which logs no
-- HandleUnpublished: the node the name hashed to before is withdrawn here.
WITH previous AS (
    SELECT handle_node FROM names.published
    WHERE chain_id = $1 AND owner = $2 AND platform_id = $3
      AND handle_node <> $4
), withdrawn AS (
    UPDATE names.disclosures d SET withdrawn = true
    FROM previous
    WHERE d.chain_id = $1 AND d.handle_node = previous.handle_node
)
INSERT INTO names.published (chain_id, owner, platform_id, handle_node)
VALUES ($1, $2, $3, $4)
ON CONFLICT (chain_id, owner, platform_id) DO UPDATE SET
    handle_node = EXCLUDED.handle_node,
    updated_at = now()
