-- Hold the chain's row for a prepare. A window an earlier writer still has
-- open holds the row until it commits, so the prepare reads, and on a replay
-- clears, what that window wrote.
SELECT true FROM names.chains WHERE chain_id = $1 FOR UPDATE
