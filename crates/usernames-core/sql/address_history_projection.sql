SELECT a.chain_id, a.block_number, a.log_index, a.block_time, a.roles,
       e.tx_hash, e.kind, e.handle_node, e.platform_id, e.holder, e.id_node, e.observed_at,
       e.ceremony_version, e.verifier, e.authorization_digest,
       e.client_identifier, e.receiver, e.token, e.sender, e.claimer,
       e.recipient, e.round::text AS round, e.amount::text AS amount,
       e.released::text AS released, e.received::text AS received,
       e.refundable_at,
       v.platform_id AS node_platform_id, v.handle
FROM names.address_events a
JOIN names.events e
  ON e.chain_id = a.chain_id AND e.block_number = a.block_number
     AND e.log_index = a.log_index
LEFT JOIN names.handle_nodes v
  ON v.chain_id = e.chain_id AND v.handle_node = e.handle_node
