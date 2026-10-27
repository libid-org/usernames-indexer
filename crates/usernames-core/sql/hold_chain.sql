-- Hold the chain's row for a window while the row names the window's
-- writer: a writer a later prepare replaced opens no window, and a prepare
-- waits for an open one to commit.
SELECT true FROM names.chains WHERE chain_id = $1 AND writer = $2 FOR NO KEY UPDATE
