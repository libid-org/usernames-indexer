-- Hold a chain's users total, as a fold writing it does until it commits:
-- a fold of the chain waits here. Returns the holding session's pid.
SELECT pg_backend_pid() FROM names.stats_totals
WHERE chain_id = $1 AND measure = 'users' AND key = '\x'::bytea
FOR UPDATE
