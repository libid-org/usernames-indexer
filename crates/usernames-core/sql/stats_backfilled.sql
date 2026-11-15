-- Chain $1's journal folded through position ($2, $3).
UPDATE names.chains SET stats_block = $2, stats_log_index = $3
WHERE chain_id = $1
