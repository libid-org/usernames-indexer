-- Announce every change to a chain's row on the channel `names_chains`, with
-- the chain's id, once its transaction commits. Under a lock, because two
-- suites replacing one function at once fail with "tuple concurrently
-- updated".
BEGIN;
SELECT pg_advisory_xact_lock(hashtextextended('usernames-fixtures:announce_chains', 0));
CREATE OR REPLACE FUNCTION names.announce_chain() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('names_chains', NEW.chain_id::text);
    RETURN NULL;
END
$$;
CREATE OR REPLACE TRIGGER chains_announced
    AFTER INSERT OR UPDATE ON names.chains
    FOR EACH ROW EXECUTE FUNCTION names.announce_chain();
COMMIT;
