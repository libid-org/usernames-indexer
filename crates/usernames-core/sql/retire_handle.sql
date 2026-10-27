-- Mirrors the contract: the owner clears; the observed-at watermark, the id
-- back-pointer and the ceremony version stay. The contract never stored the
-- version; this row keeps which one proved the binding.
UPDATE names.handles SET owner = NULL
WHERE chain_id = $1 AND handle_node = $2
