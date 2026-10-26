INSERT INTO names.chains (chain_id, proof_verifier) VALUES ($1, $2)
ON CONFLICT (chain_id) DO UPDATE SET proof_verifier = EXCLUDED.proof_verifier
