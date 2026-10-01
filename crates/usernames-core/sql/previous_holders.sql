-- Who held the handle and the account before a bind overwrites them: NULL
-- for a node never bound, and for a handle that retired.
SELECT (SELECT owner FROM names.handles WHERE chain_id = $1 AND handle_node = $2),
       (SELECT owner FROM names.ids WHERE chain_id = $1 AND id_node = $3)
