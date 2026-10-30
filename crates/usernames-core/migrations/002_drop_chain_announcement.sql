-- Readers follow a chain by reading its row.
DROP TRIGGER IF EXISTS chains_announced ON names.chains;
DROP FUNCTION IF EXISTS names.announce_chain();
