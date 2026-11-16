-- Whether the session named $1 in this database waits on a lock the session
-- with pid $2 holds.
SELECT EXISTS (
    SELECT 1 FROM pg_stat_activity
    WHERE wait_event_type = 'Lock'
      AND datname = current_database()
      AND application_name = $1
      AND $2 = ANY (pg_blocking_pids(pid))
)
