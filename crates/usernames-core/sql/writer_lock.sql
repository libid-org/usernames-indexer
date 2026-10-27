-- Wait for the chain's writer lock.
SELECT pg_advisory_lock(hashtextextended($1, 0))
