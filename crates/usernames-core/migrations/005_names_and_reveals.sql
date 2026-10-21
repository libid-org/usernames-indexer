-- One name per holder, on whichever platform, and one journal kind for
-- handle text from every contract: `handle_revealed` replaces
-- `deposited_to_handle`. The read model replays, so old rows and cursors go.

TRUNCATE names.events, names.ids, names.handles, names.published,
         names.disclosures, names.handle_nodes, names.escrow_held,
         names.escrow_refundable, names.address_events, names.handle_events;
DELETE FROM names.chain_metadata WHERE key = 'cursor' OR key = 'window_error';

ALTER TABLE names.events DROP CONSTRAINT IF EXISTS events_kind_check;
ALTER TABLE names.events ADD CONSTRAINT events_kind_check CHECK (kind IN (
    'identity_bound', 'handle_revealed', 'handle_published', 'handle_retired',
    'handle_unpublished', 'proof_verifier_configured', 'ceremony_bound',
    'bind_fee_paid', 'deposited', 'forwarded', 'claimed', 'refunded'
));

-- `published` is keyed by the holder alone.
ALTER TABLE names.published DROP CONSTRAINT IF EXISTS published_pkey;
ALTER TABLE names.published ADD PRIMARY KEY (chain_id, owner);
