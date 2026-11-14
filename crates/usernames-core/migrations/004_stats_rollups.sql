-- The rollups /v1/stats and /v1/stats/days read. Each indexer folds the
-- journal rows a window adds into them as the window commits, so a read
-- sums at most a chain's days and never its events. A UTC day is named by
-- its first second.
--
-- A row counts one measure under one key: `users` (no key), a chain's share
-- of `all_chains_users` (no key), `links` per platform id, `fees` per
-- receiver, and each escrow flow per token; fees and flows also count
-- every receiver or token together under no key. A wallet counts once per day under a measure and key, and is new
-- on its first day there.

-- The journal's reads and writes queue behind the locks below: wait 5 s at
-- most, and a migration that does not get them fails, for the indexer's
-- next start to try again. LOCAL: it ends with the migration's transaction.
SET LOCAL lock_timeout = '5s';

CREATE DOMAIN names.stats_measure AS TEXT CHECK (VALUE IN (
    'users', 'all_chains_users', 'links', 'fees',
    'deposited', 'forwarded', 'claimed', 'refunded', 'onboarding'
));

-- Whether a chain's journal is folded into the rollups, and how far the
-- fold of a journal written before them got: the position of the last row
-- it folded, (-1, -1) before the first. Its indexer folds the rest page by
-- page, each page with this position, before it indexes. A chain row
-- written from now on starts empty, folded.
ALTER TABLE names.chains
    ADD COLUMN IF NOT EXISTS stats_folded    BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS stats_block     BIGINT  NOT NULL DEFAULT -1,
    ADD COLUMN IF NOT EXISTS stats_log_index BIGINT  NOT NULL DEFAULT -1;
ALTER TABLE names.chains ALTER COLUMN stats_folded SET DEFAULT true;

-- Per chain, measure, UTC day and key: the rows counted, the amount they
-- moved, the distinct wallets that made them that day, and of them those
-- new. A day's rows of one measure sit together, so a page of days seeks
-- each day's keys it names and never meets the other keys.
CREATE TABLE IF NOT EXISTS names.stats_counts (
    chain_id    BIGINT              NOT NULL,
    measure     names.stats_measure NOT NULL,
    day         BIGINT              NOT NULL,
    key         BYTEA               NOT NULL,
    count       BIGINT              NOT NULL,
    amount      NUMERIC             NOT NULL,
    wallets     BIGINT              NOT NULL,
    new_wallets BIGINT              NOT NULL,
    PRIMARY KEY (chain_id, measure, day, key)
);
-- The same over every day: `wallets` are the distinct wallets ever.
CREATE TABLE IF NOT EXISTS names.stats_totals (
    chain_id BIGINT              NOT NULL,
    measure  names.stats_measure NOT NULL,
    key      BYTEA               NOT NULL,
    count    BIGINT              NOT NULL,
    amount   NUMERIC             NOT NULL,
    wallets  BIGINT              NOT NULL,
    PRIMARY KEY (chain_id, measure, key)
);

-- Each wallet on each day it counted under a measure and key: a fold's
-- insert that finds its row is a wallet the day already counted. Across
-- every chain the rows sit under chain 0, so two chains' folds of one
-- wallet and day insert one row.
CREATE TABLE IF NOT EXISTS names.stats_wallet_days (
    chain_id BIGINT              NOT NULL,
    measure  names.stats_measure NOT NULL,
    key      BYTEA               NOT NULL,
    wallet   BYTEA               NOT NULL,
    day      BIGINT              NOT NULL,
    PRIMARY KEY (chain_id, measure, key, wallet, day)
);
-- Each wallet's first day under a measure and key, under chain 0 across
-- every chain. A fold that sees the wallet earlier moves it there and names
-- the day it left in `moved_from`, which takes the wallet off that day's
-- new ones.
CREATE TABLE IF NOT EXISTS names.stats_firsts (
    chain_id   BIGINT              NOT NULL,
    measure    names.stats_measure NOT NULL,
    key        BYTEA               NOT NULL,
    wallet     BYTEA               NOT NULL,
    day        BIGINT              NOT NULL,
    moved_from BIGINT,
    PRIMARY KEY (chain_id, measure, key, wallet)
);

-- An onboarding claim's test, per claim a fold sees: the round's first
-- deposit, then a bind of the node between it and the claim. One seek each.
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
