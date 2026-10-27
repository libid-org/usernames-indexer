-- Rows in the shape production holds them, for the index-plan test in
-- db.rs, on the chain every probe names: 5000 per table over 1000 addresses
-- and three platforms, the probes' platform among them and the probes'
-- address, node and handle nowhere. A probe's chain and platform then select
-- little and its address or node select almost nothing, the way they do in
-- production. The test loads it in a transaction that rolls back.
--
-- Held for that transaction: autovacuum, which every rolled-back load gives
-- work, cannot rewrite these tables' statistics under the plans. Other
-- sessions still read and write them.
LOCK TABLE names.handles, names.ids, names.published, names.events,
           names.address_events, names.handle_nodes,
           names.escrow_held, names.escrow_refundable
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
INSERT INTO names.address_events
    (chain_id, address, block_number, log_index, block_time, roles)
SELECT 1, substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20),
       i, 0, i, ARRAY['sender']
FROM generate_series(1, 5000) i;
INSERT INTO names.handle_nodes (chain_id, handle_node, platform_id, handle)
SELECT 1, handle_node, platform_id, md5(handle_node::text)
FROM names.handles WHERE chain_id = 1;
INSERT INTO names.escrow_held (chain_id, handle_node, token, held, round)
SELECT 1, sha256(('node' || i)::bytea),
       substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20),
       i, 0
FROM generate_series(1, 5000) i;
INSERT INTO names.escrow_refundable
    (sender, chain_id, handle_node, token, amount, refundable_at)
SELECT substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20), 1,
       sha256(('node' || i)::bytea),
       substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20), i, i
FROM generate_series(1, 5000) i;
-- Vacuum moves a GIN index's pending entries into it; the rows above wait there.
SELECT gin_clean_pending_list('names.handle_nodes_handle_trgm_idx');
ANALYZE names.handles, names.ids, names.published, names.events,
        names.address_events, names.handle_nodes,
        names.escrow_held, names.escrow_refundable;
