-- Cancel the statement of each session named $1 in this database that
-- waits on a lock a session named $2 holds; whether there was one.
SELECT coalesce(bool_or(pg_cancel_backend(waiting.pid)), false)
FROM pg_stat_activity AS waiting
JOIN pg_stat_activity AS blocker
  ON blocker.pid = ANY (pg_blocking_pids(waiting.pid))
WHERE waiting.wait_event_type = 'Lock'
  AND waiting.datname = current_database()
  AND waiting.application_name = $1
  AND blocker.application_name = $2
