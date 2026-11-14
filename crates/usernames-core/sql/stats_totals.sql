-- Every chain's totals: every key of the measures $1, and of the measures
-- $2 the keys each chain names ($3, $4: chain and key, pairwise). One seek
-- per chain and measure, or per named key. Over every day, a total's
-- wallets are all new.
SELECT t.chain_id, t.measure, t.key, NULL::bigint AS day, t.count,
       t.amount::text AS amount, t.wallets, t.wallets AS new_wallets
FROM names.chains ch
JOIN names.stats_totals t
  ON t.chain_id = ch.chain_id AND t.measure = ANY($1::text[])
UNION ALL
SELECT t.chain_id, t.measure, t.key, NULL, t.count, t.amount::text,
       t.wallets, t.wallets
FROM unnest($3::bigint[], $4::bytea[]) AS n (chain_id, key)
JOIN names.stats_totals t
  ON t.chain_id = n.chain_id AND t.measure = ANY($2::text[]) AND t.key = n.key
ORDER BY chain_id, measure, key
