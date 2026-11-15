-- Rows in the shape production holds them, for the index-plan test in
-- db.rs, on the chain every probe names: 5000 per table over 1000 addresses
-- and three platforms, the probes' platform among them and the probes'
-- address, node and handle nowhere; the escrow's slots also on four more
-- chains. The journal holds a bind and a fee per handle beside the 5000
-- deposits, and 1000 claims. A probe's chain and platform then select little
-- and its address or node select almost nothing, the way they do in
-- production. The stats rollups hold 200 days on five chains, every day 20
-- tokens in each flow, the chain's coin 0xee..ee among them, and on chain 1
-- 300 more tokens deposited every day, as anybody can write a token. The
-- index-plan test loads it in a transaction that rolls back.
--
-- Held for that transaction: autovacuum, which every rolled-back load gives
-- work, cannot rewrite these tables' statistics under the plans. Other
-- sessions still read and write them.
LOCK TABLE names.handles, names.ids, names.published, names.events,
           names.address_events, names.handle_nodes,
           names.escrow_held, names.escrow_refundable, names.referrals,
           names.stats_counts, names.stats_totals, names.stats_wallet_days,
           names.stats_firsts
    IN SHARE UPDATE EXCLUSIVE MODE;
INSERT INTO names.handles
    (chain_id, handle_node, platform_id, owner,
     observed_at, ceremony_version, id_node)
SELECT 1, sha256(('node' || i)::bytea),
       CASE i % 3 WHEN 0 THEN decode(repeat('01', 32), 'hex')
                  ELSE sha256(('platform' || i % 3)::bytea) END,
       substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20),
       1, 1, sha256(('id' || i)::bytea)
FROM generate_series(1, 5000) i;
INSERT INTO names.ids
    (chain_id, id_node, platform_id, owner,
     observed_at, ceremony_version, handle_node)
SELECT 1, id_node, platform_id, owner, 1, 1, handle_node
FROM names.handles WHERE chain_id = 1;
INSERT INTO names.published (chain_id, owner, platform_id, handle_node)
SELECT DISTINCT ON (owner) 1, owner, platform_id, handle_node
FROM names.handles WHERE chain_id = 1;
INSERT INTO names.events
    (chain_id, block_number, log_index, tx_hash, block_time, kind, handle_node,
     token, sender, round, amount, refundable_at)
SELECT 1, i, 0, sha256(('tx' || i)::bytea), i, 'deposited',
       sha256(('node' || i % 1000)::bytea),
       substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20),
       substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20), 0, i, i
FROM generate_series(1, 5000) i;
INSERT INTO names.events
    (chain_id, block_number, log_index, tx_hash, block_time, kind, handle_node,
     platform_id, holder, id_node)
SELECT 1, i, 1, sha256(('bind' || i)::bytea), i, 'identity_bound',
       sha256(('node' || i)::bytea),
       CASE i % 3 WHEN 0 THEN decode(repeat('01', 32), 'hex')
                  ELSE sha256(('platform' || i % 3)::bytea) END,
       substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20),
       sha256(('id' || i)::bytea)
FROM generate_series(1, 5000) i;
INSERT INTO names.events
    (chain_id, block_number, log_index, tx_hash, block_time, kind, handle_node,
     receiver, amount)
SELECT 1, i, 2, sha256(('fee' || i)::bytea), i, 'bind_fee_paid',
       sha256(('node' || i)::bytea),
       substring(sha256(('receiver' || i % 2)::bytea) FROM 1 FOR 20), i
FROM generate_series(1, 5000) i;
INSERT INTO names.events
    (chain_id, block_number, log_index, tx_hash, block_time, kind, handle_node,
     token, claimer, recipient, round, released)
SELECT 1, 5000 + i, 0, sha256(('claim' || i)::bytea), 5000 + i, 'claimed',
       sha256(('node' || i)::bytea),
       substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20),
       substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20),
       substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20), 0, i
FROM generate_series(1, 1000) i;
INSERT INTO names.address_events
    (chain_id, address, block_number, log_index, block_time, roles)
SELECT 1, substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20),
       i, 0, i, ARRAY['sender']
FROM generate_series(1, 5000) i;
INSERT INTO names.handle_nodes (chain_id, handle_node, platform_id, handle)
SELECT 1, handle_node, platform_id, md5(handle_node::text)
FROM names.handles WHERE chain_id = 1;
-- Escrow slots on five chains, as on mainnet, one of the tokens the probes'
-- 0xee..ee: a list on one chain and in one token then has the other chains'
-- slots of that token to pass if it seeks across them.
INSERT INTO names.escrow_held (chain_id, handle_node, token, held, round)
SELECT chain, sha256(('node' || i)::bytea),
       CASE i % 5 WHEN 0 THEN decode(repeat('ee', 20), 'hex')
                  ELSE substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20) END,
       i, 0
FROM generate_series(1, 5000) i, generate_series(1, 5) chain;
INSERT INTO names.escrow_refundable
    (sender, chain_id, handle_node, token, amount, refundable_at)
SELECT substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20), 1,
       sha256(('node' || i)::bytea),
       substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20), i, i
FROM generate_series(1, 5000) i;
-- 5000 referrals over the 1000 nodes the first 1000 handles hold, paid,
-- forwarded or held in the node's round 0, and unpaid.
INSERT INTO names.referrals
    (chain_id, block_number, log_index, block_time, referrer_node, binder,
     platform_id, handle_node, fee, share, outcome, delivery, token, round)
SELECT 1, i, 3, i, sha256(('node' || i % 1000)::bytea),
       substring(sha256(('address' || (i + 7) % 1000)::bytea) FROM 1 FOR 20),
       sha256(('platform' || i % 3)::bytea), sha256(('node' || i)::bytea),
       10000, 3000,
       CASE WHEN i % 4 = 0 THEN 'refused' ELSE 'paid' END,
       CASE i % 4 WHEN 0 THEN NULL WHEN 1 THEN 'held' ELSE 'forwarded' END,
       CASE WHEN i % 4 <> 0
            THEN substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20) END,
       CASE WHEN i % 4 = 1 THEN 0 END
FROM generate_series(1, 5000) i;
-- A holder of many handles, 0x4e..4e: the first 1000 nodes, on chains 2 and
-- 3, where nobody else holds them. Their referrals and their slots on chains
-- 4 and 5 are its; a read over its handles that pairs them up grows with the
-- square of their count.
INSERT INTO names.handles
    (chain_id, handle_node, platform_id, owner,
     observed_at, ceremony_version, id_node)
SELECT chain, sha256(('node' || i)::bytea),
       CASE i % 3 WHEN 0 THEN decode(repeat('01', 32), 'hex')
                  ELSE sha256(('platform' || i % 3)::bytea) END,
       decode(repeat('4e', 20), 'hex'), 1, 1, sha256(('id' || i)::bytea)
FROM generate_series(1, 1000) i, generate_series(2, 3) chain;
-- A holder of one handle on chain 1, 0x7a..7a, whose node holds 5000 junk
-- tokens beside the coin: a total that walked its slots would read them all.
INSERT INTO names.handles
    (chain_id, handle_node, platform_id, owner,
     observed_at, ceremony_version, id_node)
VALUES (1, sha256('junk node'::bytea), decode(repeat('01', 32), 'hex'),
        decode(repeat('7a', 20), 'hex'), 1, 1, sha256('junk id'::bytea));
INSERT INTO names.escrow_held (chain_id, handle_node, token, held, round)
SELECT 1, sha256('junk node'::bytea),
       CASE i WHEN 0 THEN decode(repeat('ee', 20), 'hex')
              ELSE substring(sha256(('junk' || i)::bytea) FROM 1 FOR 20) END,
       1 + i, 0
FROM generate_series(0, 5000) i;
-- The same node refers heavily: 10000 coin shares, half held in its round
-- 0 and half claimed, so a total that walked its referrals would read them
-- all.
INSERT INTO names.referrals
    (chain_id, block_number, log_index, block_time, referrer_node, binder,
     platform_id, handle_node, fee, share, outcome, delivery, token, round)
SELECT 1, 10000 + i, 3, 10000 + i, sha256('junk node'::bytea),
       substring(sha256(('binder' || i)::bytea) FROM 1 FOR 20),
       decode(repeat('01', 32), 'hex'), sha256(('bound' || i)::bytea),
       10000, 3000, 'paid',
       CASE WHEN i % 2 = 0 THEN 'held' ELSE 'claimed' END,
       decode(repeat('ee', 20), 'hex'), 0
FROM generate_series(1, 10000) i;
-- Vacuum moves a GIN index's pending entries into it; the rows above wait there.
SELECT gin_clean_pending_list('names.handle_nodes_handle_trgm_idx');
ANALYZE names.handles, names.ids, names.published, names.events,
        names.address_events, names.handle_nodes,
        names.escrow_held, names.escrow_refundable, names.referrals;
-- The five chains, for the reads that seek per chain.
INSERT INTO names.chains (chain_id, writer)
SELECT chain, 1 FROM generate_series(1, 5) chain
ON CONFLICT DO NOTHING;
-- The stats rollups: 200 days on five chains. Every day counts its users,
-- each chain's share of the users across chains, links on three platforms,
-- fees to two receivers and every receiver together, and each flow under
-- 20 tokens and every token together; on chain 1, 300 more tokens deposited
-- and 300 more receivers paid every day.
INSERT INTO names.stats_counts
    (chain_id, measure, day, key, count, amount, wallets, new_wallets)
SELECT chain, measure, day * 86400, key, 10, 300, 8, 1
FROM generate_series(1, 5) chain, generate_series(1, 200) day,
     (SELECT 'users' AS measure, ''::bytea AS key
      UNION ALL SELECT 'all_chains_users', ''
      UNION ALL SELECT 'links', sha256(('platform' || p)::bytea)
      FROM generate_series(1, 3) p
      UNION ALL SELECT 'fees',
                       CASE r WHEN 0 THEN ''::bytea
                              ELSE substring(sha256(('receiver' || r)::bytea)
                                             FROM 1 FOR 20) END
      FROM generate_series(0, 2) r
      UNION ALL SELECT flow,
                       CASE t WHEN -1 THEN ''::bytea
                              WHEN 0 THEN decode(repeat('ee', 20), 'hex')
                              ELSE substring(sha256(('token' || t)::bytea)
                                             FROM 1 FOR 20) END
      FROM unnest(ARRAY['deposited', 'forwarded', 'claimed', 'refunded',
                        'onboarding']) flow,
           generate_series(-1, 19) t) k;
INSERT INTO names.stats_counts
    (chain_id, measure, day, key, count, amount, wallets, new_wallets)
SELECT 1, measure, day * 86400,
       substring(sha256((measure || t)::bytea) FROM 1 FOR 20), 1, 1, 1, 1
FROM generate_series(1, 200) day, generate_series(1, 300) t,
     unnest(ARRAY['deposited', 'fees']) measure;
INSERT INTO names.stats_totals (chain_id, measure, key, count, amount, wallets)
SELECT chain_id, measure, key, sum(count), sum(amount), sum(new_wallets)
FROM names.stats_counts
WHERE chain_id BETWEEN 1 AND 5
GROUP BY chain_id, measure, key;
-- 20 wallets on every day of each chain, and across chains under chain 0,
-- each first seen on day 1.
INSERT INTO names.stats_wallet_days (chain_id, measure, key, wallet, day)
SELECT chain, measure, '',
       substring(sha256(('address' || w)::bytea) FROM 1 FOR 20), day * 86400
FROM generate_series(0, 5) chain, generate_series(1, 20) w,
     generate_series(1, 200) day,
     LATERAL (SELECT CASE chain WHEN 0 THEN 'all_chains_users'
                                ELSE 'users' END AS measure) m;
INSERT INTO names.stats_firsts (chain_id, measure, key, wallet, day)
SELECT chain, measure, '',
       substring(sha256(('address' || w)::bytea) FROM 1 FOR 20), 86400
FROM generate_series(0, 5) chain, generate_series(1, 20) w,
     LATERAL (SELECT CASE chain WHEN 0 THEN 'all_chains_users'
                                ELSE 'users' END AS measure) m;
ANALYZE names.stats_counts, names.stats_totals, names.stats_wallet_days,
        names.stats_firsts;
