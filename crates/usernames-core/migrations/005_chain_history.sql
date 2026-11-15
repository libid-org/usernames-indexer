-- /v1/history/chain/{chain}: a chain's events newest first, over its whole
-- journal or one UTC day (a block time range), in one kind, one token, or
-- both. Each shape seeks the index led by the chain, then its filters, then
-- the order, so a page reads `limit` entries however busy the day.
SET LOCAL lock_timeout = '5s';
CREATE INDEX IF NOT EXISTS events_chain_time_idx
    ON names.events (chain_id, block_time, block_number, log_index);
CREATE INDEX IF NOT EXISTS events_chain_kind_time_idx
    ON names.events (chain_id, kind, block_time, block_number, log_index);
CREATE INDEX IF NOT EXISTS events_chain_token_time_idx
    ON names.events (chain_id, token, block_time, block_number, log_index)
    WHERE token IS NOT NULL;
CREATE INDEX IF NOT EXISTS events_chain_token_kind_time_idx
    ON names.events (chain_id, token, kind, block_time, block_number, log_index)
    WHERE token IS NOT NULL;
