-- Rows in the shape production holds them, for the index-plan test in
-- db.rs, on the chain every probe names: 5000 per table over 1000 addresses
-- and three platforms, the probes' platform among them and the probes'
-- address, node and handle nowhere. A probe's chain and platform then select
-- little and its address or node select almost nothing, the way they do in
-- production. The test loads it in a transaction that rolls back.
INSERT INTO names.handles
    (chain_id, handle_node, platform_id, handle, owner,
     observed_at, ceremony_version, id_node, block_number, log_index)
SELECT 1, sha256(('node' || i)::bytea),
       CASE i % 3 WHEN 0 THEN decode(repeat('01', 32), 'hex')
                  ELSE sha256(('platform' || i % 3)::bytea) END,
       'handle' || i,
       substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20),
       1, 1, sha256(('id' || i)::bytea), i, 0
FROM generate_series(1, 5000) i;
INSERT INTO names.ids
    (chain_id, id_node, platform_id, user_id, owner,
     observed_at, ceremony_version, handle_node, block_number, log_index)
SELECT 1, id_node, platform_id, 'user' || block_number, owner,
       1, 1, handle_node, block_number, 0
FROM names.handles WHERE chain_id = 1;
INSERT INTO names.published (chain_id, owner, platform_id, handle)
SELECT DISTINCT ON (owner, platform_id) 1, owner, platform_id, handle
FROM names.handles WHERE chain_id = 1;
INSERT INTO names.events
    (chain_id, block_number, log_index, tx_hash, kind, payload)
SELECT 1, i, 0, sha256(('tx' || i)::bytea), 'deposited', '{}'
FROM generate_series(1, 5000) i;
INSERT INTO names.address_events
    (chain_id, address, block_number, log_index, block_time, roles)
SELECT 1, substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20),
       i, 0, i, ARRAY['depositor']
FROM generate_series(1, 5000) i;
INSERT INTO names.handle_events
    (chain_id, block_number, log_index, handle_node, platform_id, block_time)
SELECT 1, i, 0, sha256(('node' || i % 1000)::bytea),
       sha256('platform1'::bytea), i
FROM generate_series(1, 5000) i;
INSERT INTO names.escrow_held
    (chain_id, handle_node, token, platform_id, held, round,
     block_number, log_index)
SELECT 1, sha256(('node' || i)::bytea),
       substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20),
       sha256('platform1'::bytea), i, 0, i, 0
FROM generate_series(1, 5000) i;
INSERT INTO names.escrow_refundable
    (refund_to, chain_id, handle_node, token, round, amount)
SELECT substring(sha256(('address' || i % 1000)::bytea) FROM 1 FOR 20), 1,
       sha256(('node' || i)::bytea),
       substring(sha256(('token' || i % 5)::bytea) FROM 1 FOR 20), 0, i
FROM generate_series(1, 5000) i;
ANALYZE names.handles, names.ids, names.published, names.events,
        names.address_events, names.handle_events, names.escrow_held,
        names.escrow_refundable;
