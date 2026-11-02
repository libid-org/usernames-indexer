-- HandleEscrow's movements per chain, kind and token: what a deposit or a
-- forward sent and what a claim or a refund released, and the distinct
-- wallets that made them, the claimer of a claim and the sender of the
-- rest. Once over the whole journal (day NULL), and once per UTC day.
SELECT chain_id, day, kind, token,
       count(*) AS count,
       sum(amount)::text AS amount,
       count(DISTINCT wallet) AS wallets
FROM (
    SELECT chain_id, kind, token,
           CASE WHEN kind IN ('claimed', 'refunded') THEN released
                ELSE amount END AS amount,
           CASE kind WHEN 'claimed' THEN claimer ELSE sender END AS wallet,
           block_time - block_time % 86400 AS day
    FROM names.events
    WHERE kind IN ('deposited', 'forwarded', 'claimed', 'refunded')
) e
GROUP BY GROUPING SETS ((chain_id, kind, token), (chain_id, day, kind, token))
ORDER BY chain_id, day NULLS FIRST, kind, token
