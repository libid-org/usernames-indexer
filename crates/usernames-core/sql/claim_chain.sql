-- A new writer for a chain whose rows it keeps.
UPDATE names.chains SET writer = $2 WHERE chain_id = $1
