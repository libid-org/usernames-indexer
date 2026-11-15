-- A claim of one slot's round: every share the escrow held for the node in
-- that round is now claimed.
UPDATE names.referrals
SET delivery = $5
WHERE chain_id = $1 AND referrer_node = $2 AND token = $3 AND round = $4::numeric
  AND delivery = 'held'
