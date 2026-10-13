-- Whether a publish is a disclosing bind's: `bind` logs it right after its
-- IdentityBound for the node. A `publish` call follows no IdentityBound, since
-- a bind ends with its CeremonyBound.
SELECT EXISTS (
    SELECT 1 FROM names.events
    WHERE chain_id = $1 AND block_number = $2 AND tx_hash = $3
      AND log_index = $4 - 1 AND kind = 'identity_bound'
      AND payload->>'handleNode' = $5
)
