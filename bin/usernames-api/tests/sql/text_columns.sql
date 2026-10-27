-- Every text, bytes, JSON or array column of the names tables, and whether
-- its table is keyed by chain.
SELECT c.table_name, c.column_name, c.data_type,
       EXISTS (SELECT 1 FROM information_schema.columns k
               WHERE k.table_schema = 'names'
                 AND k.table_name = c.table_name
                 AND k.column_name = 'chain_id')
FROM information_schema.columns c
JOIN information_schema.tables t
  ON t.table_schema = c.table_schema AND t.table_name = c.table_name
WHERE c.table_schema = 'names' AND t.table_type = 'BASE TABLE'
  AND c.data_type IN ('text', 'bytea', 'jsonb', 'ARRAY')
