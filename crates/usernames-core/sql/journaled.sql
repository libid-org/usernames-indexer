-- Whether the journal holds the log at this position of the chain.
SELECT EXISTS (
    SELECT 1 FROM names.events
    WHERE chain_id = $1 AND block_number = $2 AND log_index = $3
)
