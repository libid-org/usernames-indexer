-- Hold a chain's users total, as a fold writing it does until it commits:
-- a fold of the chain waits here.
SELECT true FROM names.stats_totals
WHERE chain_id = $1 AND measure = 'users' AND key = '\x'::bytea
FOR UPDATE
