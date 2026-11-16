-- The read model for IdentityRegistry, HandleEscrow and ReferralSplitter,
-- the whole schema in one migration. Every row in it is replayed from the
-- chains, so a schema change edits this file rather than adding a migration:
-- sqlx refuses a database that applied another `001`, and the deployment
-- starts on a new empty database where every chain replays.
--
-- Everything is keyed by chain_id. One indexer process follows one chain, any
-- number of them share a database, and the API reads across all of them.
-- Identities and handles are keyed by their nodes; handle text lives in
-- handle_nodes alone.

CREATE SCHEMA names;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- One row per chain an indexer has prepared: the generation of the writer
-- that prepared it last, which every later write names; the read-model
-- version it writes and the contracts it watches; the registry's cached
-- deployment block; how far it got and how far the chain has grown; its
-- last report and window error; the Proof Verifier the registry was pointed
-- at; how far its journal is folded into the stats rollups; and the
-- ReferralSplitter it watches, with what a bind through it pays from its last
-- `FeeSet`. The API's status is read from here.
CREATE TABLE names.chains (
    chain_id          BIGINT PRIMARY KEY,
    writer            BIGINT NOT NULL CHECK (writer > 0),
    indexer_version   TEXT,
    contract          BYTEA,
    escrow            BYTEA,
    deploy_block      BIGINT CHECK (deploy_block >= 0),   -- of `contract`
    cursor            BIGINT CHECK (cursor >= 0),
    head              BIGINT CHECK (head >= 0),
    target            BIGINT CHECK (target >= 0),
    reported_at       BIGINT CHECK (reported_at >= 0),    -- unix seconds, by the database's clock
    valid_until       BIGINT CHECK (valid_until >= 0),    -- unix seconds, by the database's clock
    window_error      TEXT,
    proof_verifier    BYTEA,
    -- Whether the chain's journal is folded into the rollups, and the
    -- position of the last row folded, (-1, -1) before the first.
    stats_folded      BOOLEAN NOT NULL DEFAULT true,
    stats_block       BIGINT  NOT NULL DEFAULT -1,
    stats_log_index   BIGINT  NOT NULL DEFAULT -1,
    referral_splitter BYTEA,
    bind_fee          NUMERIC(78,0)
);
-- Writer generations, unique across every chain and every replay.
CREATE SEQUENCE names.chain_writers;

-- Every change to a chain's row is announced on the channel `names_chains`,
-- with the chain's id, once its transaction commits: a reader that follows
-- the index waits on it instead of polling the row.
CREATE FUNCTION names.announce_chain() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('names_chains', NEW.chain_id::text);
    RETURN NULL;
END
$$;
CREATE TRIGGER chains_announced
    AFTER INSERT OR UPDATE ON names.chains
    FOR EACH ROW EXECUTE FUNCTION names.announce_chain();

-- Append-only journal of every decoded contract event, in chain order. The
-- projections below are derived state; this is the audit trail that says why
-- they hold what they hold, and its primary key is the idempotency gate for
-- every projection write.
CREATE TABLE names.events (
    chain_id     BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    log_index    BIGINT NOT NULL,
    tx_hash      BYTEA  NOT NULL,
    block_time   BIGINT NOT NULL,   -- unix seconds
    kind         TEXT   NOT NULL CONSTRAINT events_kind_check CHECK (kind IN (
        'identity_bound', 'handle_revealed', 'handle_published', 'handle_retired',
        'handle_unpublished', 'proof_verifier_configured', 'ceremony_bound',
        'bind_fee_paid', 'deposited', 'forwarded', 'claimed', 'refunded',
        'referred', 'bind_fee_set'
    )),
    -- The handle the event concerns, a ceremony's and a fee's included: what
    -- a handle history lists, from the first deposit to a handle nobody
    -- holds through every bind and claim after.
    handle_node          BYTEA,
    -- The event's parameters: each kind sets the ones it carries. A
    -- `referred` row keeps the bind in the columns a bind's fee uses: the
    -- bound handle in `handle_node` and `platform_id`, the binder in
    -- `holder`, the session in `authorization_digest` and the fee in `amount`.
    platform_id          BYTEA,
    holder               BYTEA,
    id_node              BYTEA,
    observed_at          BIGINT,
    ceremony_version     INTEGER,
    verifier             BYTEA,
    authorization_digest BYTEA,
    client_identifier    BYTEA,
    receiver             BYTEA,
    token                BYTEA,
    sender               BYTEA,
    claimer              BYTEA,
    recipient            BYTEA,
    round                NUMERIC(78,0),
    amount               NUMERIC(78,0),
    released             NUMERIC(78,0),
    received             NUMERIC(78,0),
    refundable_at        BIGINT,
    referrer_node        BYTEA,
    referrer_platform    BYTEA,
    share                NUMERIC(78,0),
    outcome              TEXT,
    PRIMARY KEY (chain_id, block_number, log_index)
);
CREATE INDEX events_handle_history_idx
    ON names.events (handle_node, block_time, chain_id, block_number, log_index)
    WHERE handle_node IS NOT NULL;
-- /v1/history/chain/{chain}: a chain's events newest first, over its whole
-- journal or one UTC day (a block time range), in one kind, one token, or
-- both. Each shape seeks the index led by the chain, then its filters, then
-- the order, so a page reads `limit` entries however busy the day.
CREATE INDEX events_chain_time_idx
    ON names.events (chain_id, block_time, block_number, log_index);
CREATE INDEX events_chain_kind_time_idx
    ON names.events (chain_id, kind, block_time, block_number, log_index);
CREATE INDEX events_chain_token_time_idx
    ON names.events (chain_id, token, block_time, block_number, log_index)
    WHERE token IS NOT NULL;
CREATE INDEX events_chain_token_kind_time_idx
    ON names.events (chain_id, token, kind, block_time, block_number, log_index)
    WHERE token IS NOT NULL;
-- An onboarding claim's test, per claim a fold sees: the round's first
-- deposit, then a bind of the node between it and the claim. One seek each.
CREATE INDEX events_first_deposit_idx
    ON names.events (handle_node, chain_id, token, round, block_number, log_index)
    WHERE kind = 'deposited';
CREATE INDEX events_binds_idx
    ON names.events (handle_node, chain_id, block_number, log_index)
    WHERE kind = 'identity_bound';

-- Mirrors idBindings plus handleNodeById: one row per id node ever bound. The
-- identity's current handle node rides along because the contract stores
-- that pointer next to the binding, and every reverse lookup wants it.
CREATE TABLE names.ids (
    chain_id         BIGINT      NOT NULL,
    id_node          BYTEA       NOT NULL,   -- 32 bytes
    platform_id      BYTEA       NOT NULL,   -- 32 bytes
    owner            BYTEA       NOT NULL,   -- 20 bytes
    observed_at      BIGINT      NOT NULL,   -- shared scale, as emitted
    ceremony_version BIGINT      NOT NULL,   -- logged by the contract, never stored there
    handle_node      BYTEA       NOT NULL,   -- handleNodeById
    PRIMARY KEY (chain_id, id_node)
);
-- Reads ask by node or by holder, across every chain or one; chain_id trails
-- so the same index serves the scoped read and yields the per-chain order the
-- API returns.
CREATE INDEX ids_node_chain_idx
    ON names.ids (id_node, chain_id);
CREATE INDEX ids_owner_chain_idx
    ON names.ids (owner, chain_id);

-- Mirrors handleBindings plus idNodeByHandle: one row per handle node ever
-- bound. owner NULL mirrors the contract's address(0) after HandleRetired;
-- the observed_at watermark survives retirement, because the contract keeps
-- it to refuse older proofs from retaking the node.
CREATE TABLE names.handles (
    chain_id         BIGINT      NOT NULL,
    handle_node      BYTEA       NOT NULL,
    platform_id      BYTEA       NOT NULL,
    owner            BYTEA,                  -- NULL = retired, no longer resolves
    observed_at      BIGINT      NOT NULL,
    ceremony_version BIGINT      NOT NULL,   -- of the last binding at this node
    id_node          BYTEA       NOT NULL,   -- idNodeByHandle back-pointer
    PRIMARY KEY (chain_id, handle_node)
);
CREATE INDEX handles_node_chain_idx
    ON names.handles (handle_node, chain_id);
-- A holder's handles in node order, so the slots they hold page in the
-- order of the escrow's key; retired rows never match.
CREATE INDEX handles_owner_chain_node_idx
    ON names.handles (owner, chain_id, handle_node) WHERE owner IS NOT NULL;
-- stats_handles: the live handles in its group order.
CREATE INDEX handles_live_idx
    ON names.handles (chain_id, platform_id, owner)
    WHERE owner IS NOT NULL;

-- Every handle node an event named with its platform: a bind,
-- HandleRevealed, HandlePublished or HandleRetired. `handle` is the text a
-- HandleRevealed carried, folded and checked to hash to the node before it
-- is written, and kept whatever happens to the name or the handle after; a
-- bare deposit names no node here.
CREATE TABLE names.handle_nodes (
    chain_id    BIGINT NOT NULL,
    handle_node BYTEA  NOT NULL,
    platform_id BYTEA  NOT NULL,
    handle      TEXT,
    PRIMARY KEY (chain_id, handle_node)
);
-- Substring and fuzzy search: pg_trgm similarity and LIKE '%q%'.
CREATE INDEX handle_nodes_handle_trgm_idx
    ON names.handle_nodes USING gin (handle gin_trgm_ops);
-- A handle node's platform and text on every chain, for the histories and
-- escrow amounts asked by node.
CREATE INDEX handle_nodes_node_chain_idx
    ON names.handle_nodes (handle_node, chain_id);

-- Mirrors primaryHandles: each holder's one name, as its platform and the
-- node it hashes to, never its text. A row exists exactly while the
-- contract's entry is set: HandlePublished upserts, HandleUnpublished deletes.
CREATE TABLE names.published (
    chain_id    BIGINT      NOT NULL,
    owner       BYTEA       NOT NULL,
    platform_id BYTEA       NOT NULL,
    handle_node BYTEA       NOT NULL,
    PRIMARY KEY (chain_id, owner)
);

-- The names a chain goes by in an ENS name: the `base` in
-- `alice.x.base.handles.link`. Each chain's indexer writes its own from its
-- configuration at every start, replacing what it declared before. The
-- primary key is the name, not (chain_id, name): a name belongs to one chain
-- across the whole store, which is what lets a label mean one thing.
CREATE TABLE names.chain_names (
    name       TEXT        PRIMARY KEY,
    chain_id   BIGINT      NOT NULL
);
CREATE INDEX chain_names_chain_idx
    ON names.chain_names (chain_id);

-- Mirrors HandleEscrow's held and round: one row per (handle node, token)
-- that ever escrowed anything. Amounts are uint256, exact.
CREATE TABLE names.escrow_held (
    chain_id     BIGINT        NOT NULL,
    handle_node  BYTEA         NOT NULL,
    token        BYTEA         NOT NULL,   -- 20 bytes; EIP-7528 0xEeee…EEeE is the chain's coin
    held         NUMERIC(78,0) NOT NULL,   -- what `claim` would pay the holder now
    round        NUMERIC(78,0) NOT NULL,   -- claims so far: the round deposits book under
    PRIMARY KEY (chain_id, handle_node, token)
);
-- The unclaimed list, token by token, largest first, read backwards: across
-- every chain, and on one.
CREATE INDEX escrow_held_unclaimed_idx
    ON names.escrow_held (token, held, chain_id, handle_node) WHERE held > 0;
CREATE INDEX escrow_held_chain_unclaimed_idx
    ON names.escrow_held (chain_id, token, held, handle_node) WHERE held > 0;
-- One handle's slots still holding something, token by token.
CREATE INDEX escrow_held_node_idx
    ON names.escrow_held (handle_node, chain_id, token) WHERE held > 0;

-- Mirrors `contribution`: what each sender may take back from a slot, and
-- from when. A refund deletes its row; a claim closes the slot's round and
-- deletes all of them, the way it ends their refunds on chain, so every row
-- is in its slot's current round. Keyed by the address first, because that
-- is how it is read.
CREATE TABLE names.escrow_refundable (
    sender        BYTEA         NOT NULL,
    chain_id      BIGINT        NOT NULL,
    handle_node   BYTEA         NOT NULL,
    token         BYTEA         NOT NULL,
    amount        NUMERIC(78,0) NOT NULL,
    refundable_at BIGINT        NOT NULL,   -- unix seconds; the sender's last deposit sets it
    PRIMARY KEY (sender, chain_id, handle_node, token)
);
-- A claim closes one slot's round for every sender at once.
CREATE INDEX escrow_refundable_slot_idx
    ON names.escrow_refundable (handle_node, chain_id, token);

-- The addresses each journal row involves, one row per (event, address) with
-- every role the address plays in it: what an address history lists, newest
-- first. The key pages one chain; the index pages every chain.
CREATE TABLE names.address_events (
    chain_id     BIGINT NOT NULL,
    address      BYTEA  NOT NULL,
    block_time   BIGINT NOT NULL,   -- unix seconds
    block_number BIGINT NOT NULL,
    log_index    BIGINT NOT NULL,
    roles        TEXT[] NOT NULL,
    PRIMARY KEY (chain_id, address, block_time, block_number, log_index)
);
CREATE INDEX address_events_history_idx
    ON names.address_events (address, block_time, chain_id, block_number, log_index);

-- One row per `Referred`: a bind that named a referrer and what the
-- referrer's share came to. A paid share keeps its token and where the escrow
-- put it: forwarded to the referrer's holder, or held for the node in a slot's
-- round until the holder's claim of that round makes it claimed. Amounts are
-- uint256, exact.
CREATE TABLE names.referrals (
    chain_id          BIGINT        NOT NULL,
    block_number      BIGINT        NOT NULL,
    log_index         BIGINT        NOT NULL,
    block_time        BIGINT        NOT NULL,   -- unix seconds
    referrer_node     BYTEA         NOT NULL,
    binder            BYTEA         NOT NULL,
    platform_id       BYTEA         NOT NULL,
    handle_node       BYTEA         NOT NULL,
    fee               NUMERIC(78,0) NOT NULL,
    share             NUMERIC(78,0) NOT NULL,
    outcome           TEXT          NOT NULL
        CHECK (outcome IN ('paid', 'self_referral', 'refused')),
    delivery          TEXT
        CHECK (delivery IN ('forwarded', 'held', 'claimed')),
    token             BYTEA,
    round             NUMERIC(78,0),
    CHECK ((outcome = 'paid') = (delivery IS NOT NULL)),
    CHECK ((outcome = 'paid') = (token IS NOT NULL)),
    CHECK ((round IS NOT NULL) = (delivery IS NOT NULL AND delivery <> 'forwarded')),
    PRIMARY KEY (chain_id, block_number, log_index)
);
-- A referrer's history across every chain, newest first, read backwards.
CREATE INDEX referrals_referrer_idx
    ON names.referrals (referrer_node, block_time, chain_id, block_number, log_index);
-- The shares one slot's round holds, which its claim settles.
CREATE INDEX referrals_held_idx
    ON names.referrals (chain_id, referrer_node, token, round)
    WHERE delivery = 'held';

-- The rollups /v1/stats and /v1/stats/days read. Each indexer folds the
-- journal rows a window adds into them as the window commits, so a read
-- sums at most a chain's days and never its events. A UTC day is named by
-- its first second.
--
-- A row counts one measure under one key: `users` (no key), a chain's share
-- of `all_chains_users` (no key), `links` per platform id, `fees` per
-- receiver, each escrow flow per token, and the paid referral shares the
-- escrow forwarded or held at the bind, `shared_forwarded` and
-- `shared_escrowed` (no key); fees and flows also count every receiver or
-- token together under no key. A wallet counts once per day under a measure
-- and key, and is new on its first day there.
CREATE DOMAIN names.stats_measure AS TEXT CHECK (VALUE IN (
    'users', 'all_chains_users', 'links', 'fees',
    'deposited', 'forwarded', 'claimed', 'refunded', 'onboarding',
    'shared_forwarded', 'shared_escrowed'
));

-- Per chain, measure, UTC day and key: the rows counted, the amount they
-- moved, the distinct wallets that made them that day, and of them those
-- new. A day's rows of one measure sit together, so a page of days seeks
-- each day's keys it names and never meets the other keys.
CREATE TABLE names.stats_counts (
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
CREATE TABLE names.stats_totals (
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
CREATE TABLE names.stats_wallet_days (
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
CREATE TABLE names.stats_firsts (
    chain_id   BIGINT              NOT NULL,
    measure    names.stats_measure NOT NULL,
    key        BYTEA               NOT NULL,
    wallet     BYTEA               NOT NULL,
    day        BIGINT              NOT NULL,
    moved_from BIGINT,
    PRIMARY KEY (chain_id, measure, key, wallet)
);
