SELECT i.chain_id, i.platform_id, i.id_node, i.owner,
       i.observed_at, i.ceremony_version, i.handle_node,
       v.handle, h.owner AS handle_owner, h.id_node AS handle_id_node,
       (p.handle_node IS NOT NULL) AS published
FROM names.ids i
LEFT JOIN names.handles h
  ON h.chain_id = i.chain_id AND h.handle_node = i.handle_node
LEFT JOIN names.disclosures v
  ON v.chain_id = i.chain_id AND v.handle_node = i.handle_node
LEFT JOIN names.published p
  ON p.chain_id = i.chain_id AND p.owner = i.owner
     AND p.platform_id = i.platform_id AND p.handle_node = i.handle_node
