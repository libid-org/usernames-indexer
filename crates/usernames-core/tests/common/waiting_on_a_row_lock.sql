-- Whether another session in this database waits to lock a chain's row for
-- a prepare.
SELECT EXISTS (
    SELECT 1 FROM pg_stat_activity
    WHERE wait_event_type = 'Lock'
      AND datname = current_database()
      AND query LIKE '%names.chains%FOR UPDATE%'
      AND pid <> pg_backend_pid()
)
