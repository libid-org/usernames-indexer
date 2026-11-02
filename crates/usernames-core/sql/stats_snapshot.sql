-- One snapshot for every number /v1/stats serves, so they agree with each
-- other whatever an indexer commits while they are read.
BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY
