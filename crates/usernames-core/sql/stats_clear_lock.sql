-- Taken by a clear of the rollups, alone: it recounts the users across
-- every chain, and no fold may move them meanwhile (stats_fold_lock.sql).
-- Held until the transaction ends.
SELECT pg_advisory_xact_lock(hashtextextended('names.stats_rollups', 0))
