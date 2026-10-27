-- Whether a session in this database waits to take a chain's writer lock.
SELECT EXISTS (
    SELECT 1 FROM pg_stat_activity
    WHERE wait_event_type = 'Lock'
      AND datname = current_database()
      AND query LIKE '%pg_advisory_lock(%'
)
