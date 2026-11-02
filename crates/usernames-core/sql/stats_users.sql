-- The wallets that used the names per chain: linked, sent through the
-- escrow, or claimed from it. Once over the whole journal (day NULL), and
-- once per UTC day with the wallets seen that day for the first time; on
-- the whole journal those are all of them.
WITH acts AS (
    SELECT chain_id,
           block_time - block_time % 86400 AS day,
           CASE kind WHEN 'identity_bound' THEN holder
                     WHEN 'claimed' THEN claimer
                     ELSE sender END AS wallet
    FROM names.events
    WHERE kind IN ('identity_bound', 'deposited', 'forwarded', 'claimed')
), firsts AS (
    SELECT chain_id, wallet, min(day) AS first_day
    FROM acts
    GROUP BY chain_id, wallet
)
SELECT a.chain_id, a.day,
       count(DISTINCT a.wallet) AS users,
       count(DISTINCT a.wallet) FILTER (WHERE a.day = f.first_day) AS new_users
FROM acts a
JOIN firsts f USING (chain_id, wallet)
GROUP BY GROUPING SETS ((a.chain_id), (a.chain_id, a.day))
ORDER BY a.chain_id, a.day NULLS FIRST
