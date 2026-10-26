-- Mirrors the contract: the owner clears; the observed-at watermark, the id
-- back-pointer and the ceremony version stay. The contract never stored the
-- version, and this row is the only record of which one proved the binding.
UPDATE names.handles SET owner = NULL
WHERE chain_id = $1 AND handle_node = $2
