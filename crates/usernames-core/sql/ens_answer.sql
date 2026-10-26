-- What the ENS gateway answers from, read in one statement and so from one
-- snapshot: the chain's row, which says how far its index got, and who holds
-- the handle node there. The gateway answers every binding, a private one
-- too, as `resolveHandle` does.
SELECT c.chain_id, c.indexer_version, c.contract, c.escrow, c.deploy_block,
       c.cursor, c.head, c.target, c.reported_at,
       c.valid_until - EXTRACT(EPOCH FROM now())::bigint AS valid_for,
       c.window_error, c.proof_verifier,
       h.owner
FROM names.chains c
LEFT JOIN names.handles h
  ON h.chain_id = c.chain_id AND h.handle_node = $2
WHERE c.chain_id = $1
