-- Whether chain $1's journal is folded into the rollups whole ($2); a
-- journal marked not folded is folded again from its first row.
UPDATE names.chains
SET stats_folded = $2, stats_block = -1, stats_log_index = -1
WHERE chain_id = $1
