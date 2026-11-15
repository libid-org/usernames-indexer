-- The journal rows a window added, by position ($3, $4), on chain $1: what
-- stats_fold_facts.sql reads.
WITH fresh AS MATERIALIZED (
    SELECT e.*
    FROM unnest($3::bigint[], $4::bigint[]) AS f (block_number, log_index)
    JOIN names.events e
      ON e.chain_id = $1 AND e.block_number = f.block_number
         AND e.log_index = f.log_index
)
