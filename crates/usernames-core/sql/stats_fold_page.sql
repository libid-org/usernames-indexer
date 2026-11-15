-- Chain $1's journal rows after position ($3, $4) through ($5, $6), in
-- journal order: a page of a journal the rollups do not hold yet, for
-- stats_fold_facts.sql to read.
WITH fresh AS MATERIALIZED (
    SELECT *
    FROM names.events
    WHERE chain_id = $1
      AND (block_number, log_index) > ($3, $4)
      AND (block_number, log_index) <= ($5, $6)
)
