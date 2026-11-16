-- Each chain's referral splitter, on the chains that watch one: the fees
-- paid to it are collected whether the deployment lists it or not.
SELECT chain_id, referral_splitter AS address
FROM names.chains
WHERE referral_splitter IS NOT NULL
ORDER BY chain_id
