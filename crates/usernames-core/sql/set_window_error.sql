-- Writes only a change, so a healthy loop's every cycle writes nothing.
UPDATE names.chains SET window_error = $3
WHERE chain_id = $1 AND writer = $2 AND window_error IS DISTINCT FROM $3
