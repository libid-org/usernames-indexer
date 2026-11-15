-- The days before $1 that any chain counted users on, newest first, at
-- most $2: each chain's newest such days, so the page's days are among
-- them, read by one seek per chain.
SELECT DISTINCT d.day
FROM names.chains ch
CROSS JOIN LATERAL (
    SELECT day
    FROM names.stats_counts
    WHERE chain_id = ch.chain_id AND measure = 'all_chains_users' AND key = ''
      AND day < $1
    ORDER BY day DESC
    LIMIT $2
) d
ORDER BY d.day DESC
LIMIT $2
