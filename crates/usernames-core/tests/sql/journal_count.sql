-- How many events a chain's journal holds.
SELECT count(*) FROM names.events WHERE chain_id = $1
