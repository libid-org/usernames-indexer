-- Cancel the statement of each session named $1 in this database that
-- waits on a lock; whether there was one.
SELECT coalesce(bool_or(pg_cancel_backend(pid)), false)
FROM pg_stat_activity
WHERE wait_event_type = 'Lock'
  AND datname = current_database()
  AND application_name = $1
