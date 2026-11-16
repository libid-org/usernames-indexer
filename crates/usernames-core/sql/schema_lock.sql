-- Wait for the right to build or check the schema, held until the
-- transaction ends: of several indexers starting on one empty database,
-- one builds and the rest find its fingerprint.
SELECT pg_advisory_xact_lock(hashtextextended('names.schema', 0))
