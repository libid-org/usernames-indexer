-- What a replay leaves of a chain, in one statement: its handles cleared
-- and its cursor with them.
WITH handles AS (DELETE FROM names.handles WHERE chain_id = $1)
UPDATE names.chains SET cursor = NULL WHERE chain_id = $1
