-- End the session that holds a chain's writer lock in this database, as a
-- database restart, a failover or an idle timeout would.
SELECT pg_terminate_backend(l.pid)
FROM pg_locks l
JOIN pg_database d ON d.oid = l.database
WHERE l.locktype = 'advisory'
  AND d.datname = current_database()
  AND ((l.classid::bigint << 32) | l.objid::bigint)
      = hashtextextended('usernames-indexer:' || $1::text, 0)
