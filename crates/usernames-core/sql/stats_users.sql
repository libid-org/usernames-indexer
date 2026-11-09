-- The wallets that used the names: linked, sent through the escrow, or
-- claimed from it. Per chain and across every chain (chain_id NULL), each
-- once over the whole journal (day NULL) and once per UTC day with the
-- wallets seen that day for the first time: on that chain, or across every
-- chain on any of them. On the whole journal those are all of them.
-- A referral splitter any chain watches, which sends every paid share, is
-- no user.
-- Each part is its own aggregate, so each reads one covering index in
-- its order.
WITH splitters AS MATERIALIZED (
    SELECT referral_splitter AS wallet
    FROM names.chains
    WHERE referral_splitter IS NOT NULL
), cdw AS MATERIALIZED (
    SELECT DISTINCT chain_id, day, wallet
    FROM names.events
    WHERE kind IN ('identity_bound', 'deposited', 'forwarded', 'claimed')
      AND wallet IS NOT NULL
      AND wallet NOT IN (SELECT wallet FROM splitters)
), firsts AS MATERIALIZED (
    SELECT chain_id, wallet, min(day) AS first_day
    FROM names.events
    WHERE kind IN ('identity_bound', 'deposited', 'forwarded', 'claimed')
      AND wallet IS NOT NULL
      AND wallet NOT IN (SELECT wallet FROM splitters)
    GROUP BY chain_id, wallet
), anywhere AS MATERIALIZED (
    SELECT wallet, min(first_day) AS first_day FROM firsts GROUP BY wallet
), dw AS (
    SELECT DISTINCT day, wallet
    FROM names.events
    WHERE kind IN ('identity_bound', 'deposited', 'forwarded', 'claimed')
      AND wallet IS NOT NULL
      AND wallet NOT IN (SELECT wallet FROM splitters)
)
SELECT c.chain_id, c.day, c.users, coalesce(n.new_users, 0) AS new_users
FROM (SELECT chain_id, day, count(*) AS users FROM cdw GROUP BY 1, 2) c
LEFT JOIN (SELECT chain_id, first_day AS day, count(*) AS new_users FROM firsts GROUP BY 1, 2) n
  USING (chain_id, day)
UNION ALL
SELECT chain_id, NULL, count(*), count(*) FROM firsts GROUP BY chain_id
UNION ALL
SELECT NULL, d.day, d.users, coalesce(n.new_users, 0)
FROM (SELECT day, count(*) AS users FROM dw GROUP BY day) d
LEFT JOIN (SELECT first_day AS day, count(*) AS new_users FROM anywhere GROUP BY 1) n USING (day)
UNION ALL
SELECT NULL, NULL, count(*), count(*) FROM anywhere
