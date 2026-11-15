-- The rollups of the days $2 on the chains $1: every key of the measures
-- $3, and the keys named by chain, measure and key ($4, $5, $6, pairwise).
-- One seek per chain, day and measure, or per day and named key. LIMIT 1
-- keeps each named key a seek of the primary key, which holds one row: as
-- a join, a long page plans as a scan of every chain's rollups.
SELECT c.chain_id, c.measure, c.key, c.day, c.count, c.amount::text AS amount,
       c.wallets, c.new_wallets
FROM unnest($1::bigint[]) AS ch (chain_id)
CROSS JOIN unnest($2::bigint[]) AS d (day)
JOIN names.stats_counts c
  ON c.chain_id = ch.chain_id AND c.measure = ANY($3::text[]) AND c.day = d.day
UNION ALL
SELECT n.chain_id, n.measure, n.key, d.day, c.count, c.amount::text,
       c.wallets, c.new_wallets
FROM unnest($4::bigint[], $5::text[], $6::bytea[]) AS n (chain_id, measure, key)
CROSS JOIN unnest($2::bigint[]) AS d (day)
CROSS JOIN LATERAL (
    SELECT c.count, c.amount, c.wallets, c.new_wallets
    FROM names.stats_counts c
    WHERE c.chain_id = n.chain_id AND c.measure = n.measure AND c.day = d.day
      AND c.key = n.key
    LIMIT 1) c
ORDER BY chain_id, day, measure, key
