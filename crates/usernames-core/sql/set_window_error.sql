-- Writes only a change, so a healthy loop's every cycle writes nothing.
INSERT INTO names.chains (chain_id, window_error) VALUES ($1, $2)
ON CONFLICT (chain_id) DO UPDATE SET window_error = EXCLUDED.window_error
WHERE names.chains.window_error IS DISTINCT FROM EXCLUDED.window_error
