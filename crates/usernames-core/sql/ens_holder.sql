-- Who a name resolves to through the gateway. A private binding resolves
-- too, as `resolveHandle` does on chain; to answer disclosed handles only,
-- join names.visible_handles here.
SELECT owner FROM names.handles WHERE chain_id = $1 AND handle_node = $2
