-- The handle a ceremony proved: the IdentityBound its bind emitted just
-- before it, in the same transaction, for the same holder and platform. The
-- payload spells addresses and ids the way NamesEvent::payload writes them.
SELECT payload->>'handleNode'
FROM names.events
WHERE chain_id = $1 AND block_number = $2 AND tx_hash = $3
  AND kind = 'identity_bound' AND log_index < $4
  AND payload->>'holder' = $5 AND payload->>'platformId' = $6
ORDER BY log_index DESC
LIMIT 1
