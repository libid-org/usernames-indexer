-- Follows `facts` (stats_fold_facts.sql): folds them into chain $1's
-- rollups, in one statement.
--
-- A wallet's presence row on a day, inserted once, adds it to the day's
-- wallets. Its first-day row, inserted or moved to an earlier day, adds it
-- to that day's new wallets; a move takes it off the day it left. Across
-- every chain both rows sit under chain 0, and the chain whose fold inserts
-- or moves one counts it in its own rows: no rollup row is written by two
-- chains. The shared rows are taken in wallet order, presence rows first,
-- so two chains' folds over the same wallets wait for each other instead of
-- deadlocking.
, days AS (
    INSERT INTO names.stats_wallet_days (chain_id, measure, key, wallet, day)
    SELECT DISTINCT scope, measure, key, wallet, day
    FROM facts
    WHERE wallet IS NOT NULL
    ORDER BY wallet, scope, measure, key, day
    ON CONFLICT DO NOTHING
    RETURNING measure, key, day
), firsts AS (
    INSERT INTO names.stats_firsts AS f (chain_id, measure, key, wallet, day)
    SELECT scope, measure, key, wallet, min(day)
    FROM facts
    WHERE wallet IS NOT NULL
    GROUP BY wallet, scope, measure, key
    ORDER BY wallet, scope, measure, key
    ON CONFLICT (chain_id, measure, key, wallet) DO UPDATE
    SET day = excluded.day, moved_from = f.day
    WHERE excluded.day < f.day
    RETURNING measure, key, day, moved_from
), deltas AS (
    SELECT measure, key, day,
           sum(count) AS count, sum(amount) AS amount,
           sum(wallets) AS wallets, sum(new_wallets) AS new_wallets
    FROM (
        SELECT measure, key, day, count, amount,
               0 AS wallets, 0 AS new_wallets
        FROM facts
        UNION ALL
        SELECT measure, key, day, 0, 0, 1, 0
        FROM days
        UNION ALL
        SELECT measure, key, day, 0, 0, 0, 1
        FROM firsts
        UNION ALL
        SELECT measure, key, moved_from, 0, 0, 0, -1
        FROM firsts
        WHERE moved_from IS NOT NULL
    ) d
    GROUP BY measure, key, day
), counts AS (
    INSERT INTO names.stats_counts AS c
        (chain_id, measure, day, key, count, amount, wallets, new_wallets)
    SELECT $1, measure, day, key, count, amount, wallets, new_wallets
    FROM deltas
    ON CONFLICT (chain_id, measure, day, key) DO UPDATE
    SET count = c.count + excluded.count,
        amount = c.amount + excluded.amount,
        wallets = c.wallets + excluded.wallets,
        new_wallets = c.new_wallets + excluded.new_wallets
)
INSERT INTO names.stats_totals AS t (chain_id, measure, key, count, amount, wallets)
SELECT $1, measure, key, sum(count), sum(amount), sum(new_wallets)
FROM deltas
GROUP BY measure, key
ON CONFLICT (chain_id, measure, key) DO UPDATE
SET count = t.count + excluded.count,
    amount = t.amount + excluded.amount,
    wallets = t.wallets + excluded.wallets
