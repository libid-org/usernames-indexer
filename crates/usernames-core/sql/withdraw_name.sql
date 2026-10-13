-- Mirrors the contract's delete of `published`, withdraws the disclosure of
-- the node the name hashed to, and returns that node.
WITH gone AS (
    DELETE FROM names.published
    WHERE chain_id = $1 AND owner = $2 AND platform_id = $3
    RETURNING handle_node
), withdrawn AS (
    UPDATE names.disclosures d SET withdrawn = true
    FROM gone
    WHERE d.chain_id = $1 AND d.handle_node = gone.handle_node
)
SELECT handle_node FROM gone
