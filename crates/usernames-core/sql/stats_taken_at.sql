-- When the snapshot was taken, in unix seconds.
SELECT EXTRACT(EPOCH FROM now())::bigint
