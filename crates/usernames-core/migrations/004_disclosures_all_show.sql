-- Every disclosure shows, withdrawn or not; nothing tracks bound platforms.
-- The rows left are what a replay would write, so none is cleared.

DROP VIEW IF EXISTS names.handle_identities;
DROP VIEW IF EXISTS names.visible_handles;
ALTER TABLE names.disclosures DROP COLUMN IF EXISTS withdrawn;
DROP TABLE IF EXISTS names.platforms;

-- Each disclosed handle and the identity behind it: the id node, never an id.
-- id_node and holder are NULL until a bind takes the node.
CREATE OR REPLACE VIEW names.handle_identities AS
SELECT d.chain_id, d.platform_id, d.handle, d.handle_node, h.id_node,
       h.owner AS holder
FROM names.disclosures d
LEFT JOIN names.handles h
  ON h.chain_id = d.chain_id AND h.handle_node = d.handle_node;
