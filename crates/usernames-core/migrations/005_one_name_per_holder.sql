-- One name per holder, on whichever platform: `published` is keyed by the
-- holder alone. Rows keyed by platform do not fit that key, so they go;
-- INDEXER_VERSION 5 replays every chain, which writes them again.

TRUNCATE names.published;
ALTER TABLE names.published DROP CONSTRAINT IF EXISTS published_pkey;
ALTER TABLE names.published ADD PRIMARY KEY (chain_id, owner);
