-- The ceremony generation of IdentityNames (libid-contracts 0.13): a binding
-- carries the ceremony version that proved it, the contract keeps no version
-- set of its own, and the events the per-version verifier tables mirrored no
-- longer exist. Like 001 and 002, everything here is re-appliable.

-- Rows of kinds this build no longer decodes cannot satisfy the new check;
-- the replay that follows the read-model version bump would drop them anyway.
DELETE FROM names.events
 WHERE kind IN ('verifier_configured', 'verifier_retired', 'latest_version_changed');
ALTER TABLE names.events DROP CONSTRAINT IF EXISTS events_kind_check;
ALTER TABLE names.events ADD CONSTRAINT events_kind_check CHECK (kind IN (
    'identity_bound', 'handle_retired', 'name_unpublished',
    'platform_configured', 'proof_verifier_configured',
    'ceremony_bound', 'claim_fee_paid'
));

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.columns
               WHERE table_schema = 'names' AND table_name = 'ids'
                 AND column_name = 'version') THEN
        ALTER TABLE names.ids RENAME COLUMN version TO ceremony_version;
    END IF;
    IF EXISTS (SELECT 1 FROM information_schema.columns
               WHERE table_schema = 'names' AND table_name = 'handles'
                 AND column_name = 'version') THEN
        ALTER TABLE names.handles RENAME COLUMN version TO ceremony_version;
    END IF;
END
$$;

ALTER TABLE names.platforms DROP COLUMN IF EXISTS latest_version;
DROP TABLE IF EXISTS names.verifiers;
