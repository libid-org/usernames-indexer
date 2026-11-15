-- The journal's kind check, with `referred`, holds for every row. VALIDATE
-- reads the journal under a lock that lets reads and writes go on, which is
-- why it runs apart from 006, whose catalog changes lock the table.
ALTER TABLE names.events VALIDATE CONSTRAINT events_kind_check;
