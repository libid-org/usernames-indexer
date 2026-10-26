SELECT e.chain_id, e.block_number, e.log_index, e.block_time,
       NULL::text[] AS roles, e.tx_hash, e.kind, e.handle_node, e.platform_id, e.holder, e.id_node, e.observed_at,
       e.ceremony_version, e.verifier, e.authorization_digest,
       e.client_identifier, e.receiver, e.token, e.sender, e.claimer,
       e.recipient, e.round::text AS round, e.amount::text AS amount,
       e.released::text AS released, e.received::text AS received,
       e.refundable_at,
       v.platform_id AS node_platform_id, v.handle
FROM names.events e
LEFT JOIN names.handle_nodes v
  ON v.chain_id = e.chain_id AND v.handle_node = e.handle_node
