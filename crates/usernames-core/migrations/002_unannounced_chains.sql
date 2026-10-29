-- Changes to a chain's row go unannounced: a reader that follows the index
-- reads the row. A test harness that waits on the changes installs its own
-- announcement.
DROP TRIGGER IF EXISTS chains_announced ON names.chains;
DROP FUNCTION IF EXISTS names.announce_chain();
