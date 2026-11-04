-- Registration fees per chain and receiver: once over the whole journal
-- (day NULL), and once per UTC day.
SELECT chain_id, day, receiver,
       count(*) AS count,
       sum(amount)::text AS amount
FROM names.events
WHERE kind = 'bind_fee_paid'
GROUP BY GROUPING SETS ((chain_id, receiver), (chain_id, day, receiver))
ORDER BY chain_id, day NULLS FIRST, receiver
