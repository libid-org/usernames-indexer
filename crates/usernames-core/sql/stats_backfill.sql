-- How far chain $1's journal is folded into the rollups, while it is not
-- whole: the position of the last row folded, (-1, -1) before the first.
SELECT stats_block AS block_number, stats_log_index AS log_index
FROM names.chains
WHERE chain_id = $1 AND NOT stats_folded
