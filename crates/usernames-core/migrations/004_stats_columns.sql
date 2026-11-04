-- The UTC day of every event and the one wallet its kind names, stored, so
-- the /v1/stats aggregates read covering indexes instead of the journal.
-- `wallet` is the holder of a bind, the claimer of a claim and the sender of
-- every other kind (NULL where the kind names none).
--
-- Adding the columns rewrites the journal under an exclusive lock, which
-- every read of it queues behind. The lock is waited for 5 s at most: a
-- migration that does not get it fails, and the indexer's next start tries
-- again. LOCAL: the setting ends with the migration's transaction instead
-- of staying on the pooled connection.
SET LOCAL lock_timeout = '5s';
ALTER TABLE names.events
    ADD COLUMN IF NOT EXISTS day BIGINT
        GENERATED ALWAYS AS (block_time - block_time % 86400) STORED,
    ADD COLUMN IF NOT EXISTS wallet BYTEA
        GENERATED ALWAYS AS (CASE kind WHEN 'identity_bound' THEN holder
                                       WHEN 'claimed' THEN claimer
                                       ELSE sender END) STORED;
-- An index-only scan skips the heap only on pages vacuum marked all-visible;
-- the journal only grows, so vacuum it after every 1% of new rows.
ALTER TABLE names.events SET (autovacuum_vacuum_insert_scale_factor = 0.01);

-- stats_users: each wallet's first day per chain, and the wallets of each
-- day, on one chain and across every chain.
CREATE INDEX IF NOT EXISTS events_users_wallet_idx
    ON names.events (chain_id, wallet, day)
    WHERE kind IN ('identity_bound', 'deposited', 'forwarded', 'claimed');
CREATE INDEX IF NOT EXISTS events_users_day_idx
    ON names.events (chain_id, day, wallet)
    WHERE kind IN ('identity_bound', 'deposited', 'forwarded', 'claimed');
CREATE INDEX IF NOT EXISTS events_users_all_chains_day_idx
    ON names.events (day, wallet)
    WHERE kind IN ('identity_bound', 'deposited', 'forwarded', 'claimed');
-- stats_links, stats_fees and stats_escrow, each read index-only.
CREATE INDEX IF NOT EXISTS events_links_idx
    ON names.events (chain_id, platform_id, day) INCLUDE (holder)
    WHERE kind = 'identity_bound';
CREATE INDEX IF NOT EXISTS events_fees_idx
    ON names.events (chain_id, receiver, day) INCLUDE (amount)
    WHERE kind = 'bind_fee_paid';
CREATE INDEX IF NOT EXISTS events_escrow_idx
    ON names.events (chain_id, kind, token, day)
    INCLUDE (amount, released, wallet)
    WHERE kind IN ('deposited', 'forwarded', 'claimed', 'refunded');
-- stats_onboarding: the claims read index-only, each claim's first deposit
-- and the bind after it one seek each.
CREATE INDEX IF NOT EXISTS events_claims_idx
    ON names.events (chain_id, token, day)
    INCLUDE (released, wallet, handle_node, round, block_number, log_index)
    WHERE kind = 'claimed';
CREATE INDEX IF NOT EXISTS events_first_deposit_idx
    ON names.events (handle_node, chain_id, token, round, block_number, log_index)
    WHERE kind = 'deposited';
CREATE INDEX IF NOT EXISTS events_binds_idx
    ON names.events (handle_node, chain_id, block_number, log_index)
    WHERE kind = 'identity_bound';
-- stats_handles: the live handles in its group order.
CREATE INDEX IF NOT EXISTS handles_live_idx
    ON names.handles (chain_id, platform_id, owner)
    WHERE owner IS NOT NULL;
