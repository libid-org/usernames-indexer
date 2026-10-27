-- Whether another session in this database waits on a lock.
SELECT EXISTS (
    SELECT 1 FROM pg_stat_activity
    WHERE wait_event_type = 'Lock'
      AND datname = current_database()
      AND pid <> pg_backend_pid()
)
