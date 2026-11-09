-- HandleEscrow's movements per chain, kind and token: what a deposit or a
-- forward sent and what a claim or a refund released, and the distinct
-- wallets that made them, the claimer of a claim and the sender of the
-- rest. A referral splitter any chain watches, which sends every paid share,
-- counts among the movements but not among the wallets. Once over the whole
-- journal (day NULL), and once per UTC day.
-- A token is named only when it is the chain's coin ($1) or a listed
-- (chain, token) pair ($2, $3); every other token folds into one row with
-- no token and no amount, since anybody can write a token and a sum across
-- tokens means nothing.
WITH listed (chain_id, token) AS (
    SELECT * FROM unnest($2::bigint[], $3::bytea[])
), splitters AS MATERIALIZED (
    SELECT referral_splitter AS wallet
    FROM names.chains
    WHERE referral_splitter IS NOT NULL
)
SELECT chain_id, day, kind, token,
       count(*) AS count,
       CASE WHEN token IS NOT NULL THEN sum(amount)::text END AS amount,
       count(DISTINCT wallet) AS wallets
FROM (
    SELECT chain_id, kind,
           CASE WHEN token = $1
                  OR (chain_id, token) IN (SELECT chain_id, token FROM listed)
                THEN token END AS token,
           CASE WHEN kind IN ('claimed', 'refunded') THEN released
                ELSE amount END AS amount,
           CASE WHEN wallet NOT IN (SELECT wallet FROM splitters)
                THEN wallet END AS wallet,
           day
    FROM names.events
    WHERE kind IN ('deposited', 'forwarded', 'claimed', 'refunded')
) e
GROUP BY GROUPING SETS ((chain_id, kind, token), (chain_id, day, kind, token))
ORDER BY chain_id, day NULLS FIRST, kind, token NULLS LAST
