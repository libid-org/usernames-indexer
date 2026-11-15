-- The referral behind each history entry that has one, by the entry's key.
-- A `referred` entry is its own referral ($1–$3). A deposit or forward the
-- chain's referral splitter sent ($4–$8: key, paid node, sender) pays the
-- first referral after it in its block for that node, as the fold pairs
-- them. Each link names the binder and the binder's published handle on the
-- referral's chain as it stands. The caller appends the referrer handle's
-- text as `rv`.
SELECT l.chain_id, l.block_number, l.log_index, l.referral_log_index,
       l.referrer_node, rv.handle AS referrer_handle, l.binder,
       v.handle AS binder_handle
FROM (
    SELECT r.chain_id, r.block_number, r.log_index,
           r.log_index AS referral_log_index, r.referrer_node, r.binder
    FROM unnest($1::bigint[], $2::bigint[], $3::bigint[])
         AS k (chain_id, block_number, log_index)
    JOIN names.referrals r
      ON r.chain_id = k.chain_id AND r.block_number = k.block_number
     AND r.log_index = k.log_index
    UNION ALL
    SELECT k.chain_id, k.block_number, k.log_index,
           r.log_index, r.referrer_node, r.binder
    FROM unnest($4::bigint[], $5::bigint[], $6::bigint[], $7::bytea[], $8::bytea[])
         AS k (chain_id, block_number, log_index, handle_node, sender)
    JOIN names.chains c
      ON c.chain_id = k.chain_id AND c.referral_splitter = k.sender
    CROSS JOIN LATERAL (
        SELECT r.log_index, r.referrer_node, r.binder
        FROM names.referrals r
        WHERE r.chain_id = k.chain_id AND r.block_number = k.block_number
          AND r.log_index > k.log_index AND r.referrer_node = k.handle_node
        ORDER BY r.log_index
        LIMIT 1
    ) r
) l
LEFT JOIN names.published p
  ON p.chain_id = l.chain_id AND p.owner = l.binder
LEFT JOIN names.handle_nodes v
  ON v.chain_id = p.chain_id AND v.handle_node = p.handle_node
