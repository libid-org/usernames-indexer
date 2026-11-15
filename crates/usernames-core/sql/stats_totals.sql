-- Every chain's totals: every key of the measures $1, and the keys named
-- by chain, measure and key ($2, $3, $4, pairwise). One seek per chain and
-- measure, or per named key: LIMIT 1 keeps each a seek of the primary key,
-- which holds one row. Over every day, a total's wallets are all new.
SELECT t.chain_id, t.measure, t.key, NULL::bigint AS day, t.count,
       t.amount::text AS amount, t.wallets, t.wallets AS new_wallets
FROM names.chains ch
JOIN names.stats_totals t
  ON t.chain_id = ch.chain_id AND t.measure = ANY($1::text[])
UNION ALL
SELECT n.chain_id, n.measure, n.key, NULL, t.count, t.amount::text,
       t.wallets, t.wallets
FROM unnest($2::bigint[], $3::text[], $4::bytea[]) AS n (chain_id, measure, key)
CROSS JOIN LATERAL (
    SELECT t.count, t.amount, t.wallets
    FROM names.stats_totals t
    WHERE t.chain_id = n.chain_id AND t.measure = n.measure AND t.key = n.key
    LIMIT 1) t
ORDER BY chain_id, measure, key
