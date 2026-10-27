-- Whether another session waits for a row a FOR UPDATE asked for.
SELECT EXISTS (
    SELECT 1 FROM pg_stat_activity
    WHERE wait_event_type = 'Lock'
      AND query LIKE '%FOR UPDATE%'
      AND pid <> pg_backend_pid()
)
