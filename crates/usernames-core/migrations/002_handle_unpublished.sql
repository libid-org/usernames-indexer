-- The journal kind for HandleUnpublished, which IdentityRegistry 0.15 logs
-- where IdentityNames logged NameUnpublished. A deployment from before 0.15
-- still logs the old event, so the journal admits both kinds.
ALTER TABLE names.events DROP CONSTRAINT IF EXISTS events_kind_check;
ALTER TABLE names.events ADD CONSTRAINT events_kind_check CHECK (kind IN (
    'identity_bound', 'handle_retired', 'handle_unpublished', 'name_unpublished',
    'platform_configured', 'proof_verifier_configured',
    'ceremony_bound', 'bind_fee_paid'
));
