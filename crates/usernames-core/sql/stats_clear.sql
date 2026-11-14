-- Chain $1's rollups, and the users across every chain, in one statement:
-- every chain's share of them and the rows under chain 0. The chains left
-- fold their journals into those again after it (stats_refolds.sql).
WITH counts AS (
         DELETE FROM names.stats_counts
         WHERE chain_id = $1 OR measure = 'all_chains_users'),
     totals AS (
         DELETE FROM names.stats_totals
         WHERE chain_id = $1 OR measure = 'all_chains_users'),
     wallet_days AS (
         DELETE FROM names.stats_wallet_days WHERE chain_id IN ($1, 0)),
     firsts AS (
         DELETE FROM names.stats_firsts WHERE chain_id IN ($1, 0))
SELECT 1
