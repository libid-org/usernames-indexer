-- Chain $1's days before $2, newest first, at most $3: every day it
-- journaled something counts its users.
SELECT day
FROM names.stats_counts
WHERE chain_id = $1 AND measure = 'users' AND key = '' AND day < $2
ORDER BY day DESC
LIMIT $3
