-- When a handle's latest proof read the platform: its stale-proof watermark.
SELECT observed_at FROM names.handles WHERE chain_id = $1 AND handle_node = $2
