-- End the session that holds a chain's writer lock, as a database restart,
-- a failover or an idle timeout would.
SELECT pg_terminate_backend(pid)
FROM pg_locks
WHERE locktype = 'advisory'
  AND ((classid::bigint << 32) | objid::bigint)
      = hashtextextended('usernames-indexer:' || $1::text, 0)
