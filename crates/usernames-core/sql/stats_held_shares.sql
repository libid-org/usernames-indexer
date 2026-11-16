-- Per chain, the paid referral shares the escrow holds now, unclaimed:
-- how many and their sum in the chain's coin. The partial index holds only
-- these rows, so the read never meets a forwarded or claimed share.
SELECT chain_id, count(*) AS count, SUM(share)::text AS amount
FROM names.referrals
WHERE delivery = 'held'
GROUP BY chain_id
ORDER BY chain_id
