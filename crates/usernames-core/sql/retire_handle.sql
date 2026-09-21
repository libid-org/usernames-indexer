-- Mirrors the contract: the owner clears; the observed-at watermark, the id
-- back-pointer and the ceremony version stay. The contract never stored the
-- version, and this row is the only record of which one proved the binding.
UPDATE names.handles SET
    owner = NULL,
    block_number = $3,
    log_index = $4,
    updated_at = now()
WHERE chain_id = $1 AND handle_node = $2
