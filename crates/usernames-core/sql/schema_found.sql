-- Whether the database holds the `names` schema, and whether that schema
-- keeps a fingerprint.
SELECT to_regnamespace('names') IS NOT NULL AS built,
       to_regclass('names.schema') IS NOT NULL AS fingerprinted
