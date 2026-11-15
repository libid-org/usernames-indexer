-- Taken by every fold, shared: folds of several chains run together, and a
-- clear of the rollups (stats_clear_lock.sql) waits for them and holds
-- them off until it commits. Held until the transaction ends.
SELECT pg_advisory_xact_lock_shared(hashtextextended('names.stats_rollups', 0))
