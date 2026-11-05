-- Referrals: the splitter a chain's indexer watches, its `Referred` events
-- in the journal, and their projection by referrer.
--
-- Every statement here takes the journal's exclusive lock only for a catalog
-- change, which is waited for 5 s at most: a migration that does not get it
-- fails, and the indexer's next start tries again. The new kind's check is
-- added NOT VALID, so no row is read under the lock; 006 validates it.
SET LOCAL lock_timeout = '5s';

-- The ReferralSplitter the chain's indexer watches, beside its escrow.
ALTER TABLE names.chains ADD COLUMN IF NOT EXISTS referrals BYTEA;

-- A `referred` row keeps the bind in the columns a bind's fee uses: the
-- bound handle in `handle_node` and `platform_id`, the binder in `holder`,
-- the session in `authorization_digest` and the fee in `amount`.
ALTER TABLE names.events
    ADD COLUMN IF NOT EXISTS referrer_node     BYTEA,
    ADD COLUMN IF NOT EXISTS referrer_platform BYTEA,
    ADD COLUMN IF NOT EXISTS share             NUMERIC(78,0),
    ADD COLUMN IF NOT EXISTS outcome           TEXT;
ALTER TABLE names.events DROP CONSTRAINT IF EXISTS events_kind_check;
ALTER TABLE names.events ADD CONSTRAINT events_kind_check CHECK (kind IN (
    'identity_bound', 'handle_revealed', 'handle_published', 'handle_retired',
    'handle_unpublished', 'proof_verifier_configured', 'ceremony_bound',
    'bind_fee_paid', 'deposited', 'forwarded', 'claimed', 'refunded',
    'referred'
)) NOT VALID;

-- One row per `Referred`: a bind that named a referrer, what the referrer's
-- share came to, and for a paid share whether the escrow forwarded it to the
-- referrer's holder or holds it for the node. Amounts are uint256, exact.
CREATE TABLE IF NOT EXISTS names.referrals (
    chain_id          BIGINT        NOT NULL,
    block_number      BIGINT        NOT NULL,
    log_index         BIGINT        NOT NULL,
    block_time        BIGINT        NOT NULL,   -- unix seconds
    referrer_node     BYTEA         NOT NULL,
    referrer_platform BYTEA         NOT NULL,
    binder            BYTEA         NOT NULL,
    platform_id       BYTEA         NOT NULL,
    handle_node       BYTEA         NOT NULL,
    fee               NUMERIC(78,0) NOT NULL,
    share             NUMERIC(78,0) NOT NULL,
    outcome           TEXT          NOT NULL
        CHECK (outcome IN ('paid', 'self_referral', 'refused')),
    delivery          TEXT
        CHECK (delivery IN ('forwarded', 'held')),
    CHECK ((outcome = 'paid') = (delivery IS NOT NULL)),
    PRIMARY KEY (chain_id, block_number, log_index)
);
-- A referrer's history across every chain, newest first, read backwards.
CREATE INDEX IF NOT EXISTS referrals_referrer_idx
    ON names.referrals (referrer_node, block_time, chain_id, block_number, log_index);
