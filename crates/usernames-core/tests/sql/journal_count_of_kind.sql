-- How many events of one kind a chain's journal holds.
SELECT count(*) FROM names.events WHERE chain_id = $1 AND kind = $2
