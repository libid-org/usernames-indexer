INSERT INTO names.chains (chain_id, deploy_block) VALUES ($1, $2)
ON CONFLICT (chain_id) DO UPDATE SET deploy_block = EXCLUDED.deploy_block
