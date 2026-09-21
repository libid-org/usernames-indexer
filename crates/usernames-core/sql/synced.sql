SELECT EXISTS(SELECT 1 FROM names.chain_metadata
              WHERE key = $1 AND ($2::bigint IS NULL OR chain_id = $2))
