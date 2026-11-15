-- Where the next page of chain $1's journal after position ($2, $3) ends:
-- its $4th row, or its last when fewer remain; no row when none does.
SELECT block_number, log_index
FROM (
    SELECT block_number, log_index
    FROM names.events
    WHERE chain_id = $1 AND (block_number, log_index) > ($2, $3)
    ORDER BY block_number, log_index
    LIMIT $4
) page
ORDER BY block_number DESC, log_index DESC
LIMIT 1
