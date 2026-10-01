-- The journal kind for HandleUnpublished, which IdentityNames 0.15 logs in
-- place of NameUnpublished. A proxy upgraded in place keeps both topics in
-- its history, so the journal admits both kinds.
ALTER TABLE names.events DROP CONSTRAINT IF EXISTS events_kind_check;
ALTER TABLE names.events ADD CONSTRAINT events_kind_check CHECK (kind IN (
    'identity_bound', 'handle_retired', 'handle_unpublished', 'name_unpublished',
    'platform_configured', 'proof_verifier_configured',
    'ceremony_bound', 'bind_fee_paid'
));
