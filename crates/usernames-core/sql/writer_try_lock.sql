-- Take the chain's writer lock if it is free.
SELECT pg_try_advisory_lock(hashtextextended($1, 0))
