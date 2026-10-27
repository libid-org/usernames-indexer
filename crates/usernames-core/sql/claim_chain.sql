-- A new writer for a chain whose rows it keeps.
UPDATE names.chains SET writer = nextval('names.chain_writers')
WHERE chain_id = $1
RETURNING writer
