-- Stops a statement of the snapshot that runs this long: the stats lock is
-- held until the computation ends, and every /v1/stats request waits on it.
SET LOCAL statement_timeout = '30s'
