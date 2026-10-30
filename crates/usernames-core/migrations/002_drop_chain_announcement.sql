-- Readers follow the index through the chain's row. A suite that waits on the
-- row's changes installs the announcement it listens to.
DROP TRIGGER IF EXISTS chains_announced ON names.chains;
DROP FUNCTION IF EXISTS names.announce_chain();
