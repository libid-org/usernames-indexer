-- The rollups of the days $2 on the chains $1: every key of the measures
-- $3, and of the measures $4 the keys each chain names ($5, $6: chain and
-- key, pairwise). One seek per chain, day and measure, or per named key.
SELECT c.chain_id, c.measure, c.key, c.day, c.count, c.amount::text AS amount,
       c.wallets, c.new_wallets
FROM unnest($1::bigint[]) AS ch (chain_id)
CROSS JOIN unnest($2::bigint[]) AS d (day)
JOIN names.stats_counts c
  ON c.chain_id = ch.chain_id AND c.measure = ANY($3::text[]) AND c.day = d.day
UNION ALL
SELECT c.chain_id, c.measure, c.key, c.day, c.count, c.amount::text,
       c.wallets, c.new_wallets
FROM unnest($5::bigint[], $6::bytea[]) AS n (chain_id, key)
CROSS JOIN unnest($2::bigint[]) AS d (day)
JOIN names.stats_counts c
  ON c.chain_id = n.chain_id AND c.measure = ANY($4::text[]) AND c.day = d.day
     AND c.key = n.key
ORDER BY chain_id, day, measure, key
