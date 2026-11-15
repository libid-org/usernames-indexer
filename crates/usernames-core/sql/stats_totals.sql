-- Every chain's totals: every key of the measures $1, and the keys named
-- by chain, measure and key ($2, $3, $4, pairwise). One seek per chain and
-- measure, or per named key: LIMIT 1 keeps each a seek of the primary key,
-- which holds one row. Each chain and measure is its own lateral seek, and
-- OFFSET 0 keeps the planner from flattening it into a join that reads the
-- whole table: both columns lead the index condition whatever it knows of
-- `names.chains`, which is too small for autovacuum to analyze. Over every
-- day, a total's wallets are all new.
SELECT c.chain_id, t.measure, t.key, NULL::bigint AS day, t.count,
       t.amount::text AS amount, t.wallets, t.wallets AS new_wallets
FROM unnest(ARRAY(SELECT chain_id FROM names.chains)) AS c (chain_id)
CROSS JOIN unnest($1::text[]) AS m (measure)
CROSS JOIN LATERAL (
    SELECT t.measure, t.key, t.count, t.amount, t.wallets
    FROM names.stats_totals t
    WHERE t.chain_id = c.chain_id AND t.measure = m.measure
    OFFSET 0) t
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
