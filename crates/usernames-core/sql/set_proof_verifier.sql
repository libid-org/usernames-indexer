-- Under the window's hold on the row.
UPDATE names.chains SET proof_verifier = $2 WHERE chain_id = $1
