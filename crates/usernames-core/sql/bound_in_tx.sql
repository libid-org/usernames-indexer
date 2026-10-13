-- Whether this transaction's bind, logged before the publish, bound the node:
-- a disclosing bind rather than a `publish`.
SELECT EXISTS (
    SELECT 1 FROM names.events
    WHERE chain_id = $1 AND block_number = $2 AND tx_hash = $3
      AND log_index < $4 AND kind = 'identity_bound'
      AND payload->>'handleNode' = $5
)
