-- Every row an indexer wrote for one chain into the read model, for a replay
-- from the deployment block: the journal, the projections, the escrow's books
-- and the histories, in one statement.
WITH events AS (DELETE FROM names.events WHERE chain_id = $1),
     ids AS (DELETE FROM names.ids WHERE chain_id = $1),
     handles AS (DELETE FROM names.handles WHERE chain_id = $1),
     published AS (DELETE FROM names.published WHERE chain_id = $1),
     platforms AS (DELETE FROM names.platforms WHERE chain_id = $1),
     escrow_held AS (DELETE FROM names.escrow_held WHERE chain_id = $1),
     escrow_refundable AS (DELETE FROM names.escrow_refundable WHERE chain_id = $1),
     address_events AS (DELETE FROM names.address_events WHERE chain_id = $1),
     handle_events AS (DELETE FROM names.handle_events WHERE chain_id = $1),
     handle_nodes AS (DELETE FROM names.handle_nodes WHERE chain_id = $1)
SELECT 1
