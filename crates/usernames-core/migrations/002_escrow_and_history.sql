-- HandleEscrow beside IdentityRegistry, and the histories of addresses and
-- handles. Nothing here is backfilled: INDEXER_VERSION 2 makes every chain
-- replay from its registry's deployment block, which writes these from the
-- first event on.

ALTER TABLE names.events DROP CONSTRAINT IF EXISTS events_kind_check;
ALTER TABLE names.events ADD CONSTRAINT events_kind_check CHECK (kind IN (
    'identity_bound', 'handle_retired', 'handle_unpublished',
    'platform_configured', 'proof_verifier_configured',
    'ceremony_bound', 'bind_fee_paid',
    'deposited', 'forwarded', 'claimed', 'refunded'
));

-- A holder's handles in node order, so the slots they hold page in the
-- order of the escrow's key.
DROP INDEX IF EXISTS names.handles_owner_chain_idx;
CREATE INDEX IF NOT EXISTS handles_owner_chain_node_idx
    ON names.handles (owner, chain_id, handle_node) WHERE owner IS NOT NULL;

-- The platform of every handle node an event has named one for: binds,
-- retirements, deposits and forwards. A node belongs to one platform, so
-- what keys by node reads it from here.
CREATE TABLE IF NOT EXISTS names.handle_nodes (
    chain_id    BIGINT NOT NULL,
    handle_node BYTEA  NOT NULL,
    platform_id BYTEA  NOT NULL,
    PRIMARY KEY (chain_id, handle_node)
);

-- Mirrors HandleEscrow's held and round: one row per (handle node, token)
-- that ever escrowed anything. Amounts are uint256, exact.
CREATE TABLE IF NOT EXISTS names.escrow_held (
    chain_id     BIGINT        NOT NULL,
    handle_node  BYTEA         NOT NULL,
    token        BYTEA         NOT NULL,   -- 20 bytes; EIP-7528 0xEeee…EEeE is the chain's coin
    held         NUMERIC(78,0) NOT NULL,   -- what `claim` would pay the holder now
    round        NUMERIC(78,0) NOT NULL,   -- claims so far: the round deposits book under
    block_number BIGINT        NOT NULL,   -- provenance: the write that produced this row
    log_index    BIGINT        NOT NULL,
    updated_at   TIMESTAMPTZ   NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, handle_node, token)
);
-- The unclaimed list, token by token, largest first, read backwards: across
-- every chain, and on one.
CREATE INDEX IF NOT EXISTS escrow_held_unclaimed_idx
    ON names.escrow_held (token, held, chain_id, handle_node) WHERE held > 0;
CREATE INDEX IF NOT EXISTS escrow_held_chain_unclaimed_idx
    ON names.escrow_held (chain_id, token, held, handle_node) WHERE held > 0;
-- One handle's slots still holding something, token by token.
CREATE INDEX IF NOT EXISTS escrow_held_node_idx
    ON names.escrow_held (handle_node, chain_id, token) WHERE held > 0;

-- Mirrors `refundable`: what each refundTo may take back from a slot in its
-- current round. A refund deletes its row; a claim closes the round and
-- deletes all of them, the way it ends their refunds on chain. Keyed by the
-- address first, because that is how it is read.
CREATE TABLE IF NOT EXISTS names.escrow_refundable (
    refund_to   BYTEA         NOT NULL,
    chain_id    BIGINT        NOT NULL,
    handle_node BYTEA         NOT NULL,
    token       BYTEA         NOT NULL,
    round       NUMERIC(78,0) NOT NULL,
    amount      NUMERIC(78,0) NOT NULL,
    updated_at  TIMESTAMPTZ   NOT NULL DEFAULT now(),
    PRIMARY KEY (refund_to, chain_id, handle_node, token, round)
);
-- A claim closes one slot's round for every refundTo at once.
CREATE INDEX IF NOT EXISTS escrow_refundable_round_idx
    ON names.escrow_refundable (handle_node, chain_id, token, round);

-- The addresses each journal row involves, one row per (event, address) with
-- every role the address plays in it: what an address history lists, newest
-- first. The key pages one chain; the index pages every chain.
CREATE TABLE IF NOT EXISTS names.address_events (
    chain_id     BIGINT NOT NULL,
    address      BYTEA  NOT NULL,
    block_time   BIGINT NOT NULL,   -- unix seconds
    block_number BIGINT NOT NULL,
    log_index    BIGINT NOT NULL,
    roles        TEXT[] NOT NULL,
    PRIMARY KEY (chain_id, address, block_time, block_number, log_index)
);
CREATE INDEX IF NOT EXISTS address_events_history_idx
    ON names.address_events (address, block_time, chain_id, block_number, log_index);

-- The handle node each journal row concerns: what a handle history lists,
-- from the first deposit to a handle nobody holds through every bind and
-- claim after. An event concerns one handle at most, so the journal
-- position is the key.
CREATE TABLE IF NOT EXISTS names.handle_events (
    chain_id     BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    log_index    BIGINT NOT NULL,
    handle_node  BYTEA  NOT NULL,
    block_time   BIGINT NOT NULL,
    PRIMARY KEY (chain_id, block_number, log_index)
);
CREATE INDEX IF NOT EXISTS handle_events_history_idx
    ON names.handle_events (handle_node, block_time, chain_id, block_number, log_index);
