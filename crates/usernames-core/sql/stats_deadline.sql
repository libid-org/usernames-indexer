-- Bounds the snapshot's next statement by the time its computation has
-- left, $1 in milliseconds: the stats lock is held until the computation
-- ends, and every /v1/stats request waits on it.
SELECT set_config('statement_timeout', $1, true)
