-- EXISTS, not a bare SELECT 1: a literal is INT4 on the wire, and decoding
-- it as i64 fails exactly when a row IS found.
SELECT EXISTS(SELECT 1 FROM names.platforms
              WHERE platform_id = $1 AND ($2::bigint IS NULL OR chain_id = $2))
