-- Bindings per chain and platform, and the distinct wallets they bound to:
-- once over the whole journal (day NULL), and once per UTC day.
SELECT chain_id, day, platform_id,
       count(*) AS count,
       count(DISTINCT holder) AS holders
FROM (
    SELECT chain_id, platform_id, holder,
           block_time - block_time % 86400 AS day
    FROM names.events
    WHERE kind = 'identity_bound'
) e
GROUP BY GROUPING SETS ((chain_id, platform_id), (chain_id, day, platform_id))
ORDER BY chain_id, day NULLS FIRST, platform_id
