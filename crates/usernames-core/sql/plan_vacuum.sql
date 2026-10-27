-- The tables the plan test loads, without the dead rows its earlier,
-- rolled-back loads left: the planner sizes a table by its pages, dead rows
-- included.
VACUUM names.handles, names.ids, names.published, names.events,
       names.address_events, names.handle_nodes,
       names.escrow_held, names.escrow_refundable
