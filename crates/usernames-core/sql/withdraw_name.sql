-- Mirrors the contract's delete of `published`, and withdraws the disclosure
-- of the node the name hashed to.
WITH gone AS (
    DELETE FROM names.published
    WHERE chain_id = $1 AND owner = $2 AND platform_id = $3
    RETURNING handle_node
)
UPDATE names.disclosures d SET withdrawn = true
FROM gone
WHERE d.chain_id = $1 AND d.handle_node = gone.handle_node
