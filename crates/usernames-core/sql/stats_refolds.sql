-- Every chain but $1, whether its journal is folded into the rollups
-- whole, and else the position of the last row folded.
SELECT chain_id, stats_folded AS folded,
       stats_block AS block_number, stats_log_index AS log_index
FROM names.chains
WHERE chain_id <> $1
ORDER BY chain_id
