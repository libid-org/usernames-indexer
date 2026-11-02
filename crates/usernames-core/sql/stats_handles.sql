-- The handles that resolve now, per chain and platform, and the distinct
-- wallets holding them.
SELECT chain_id, platform_id,
       count(*) AS live,
       count(DISTINCT owner) AS holders
FROM names.handles
WHERE owner IS NOT NULL
GROUP BY chain_id, platform_id
ORDER BY chain_id, platform_id
