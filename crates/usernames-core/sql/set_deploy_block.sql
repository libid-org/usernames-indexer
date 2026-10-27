UPDATE names.chains SET deploy_block = $3 WHERE chain_id = $1 AND writer = $2
