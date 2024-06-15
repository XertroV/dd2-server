-- Add up migration script here
-- contexts -> predecessor
CREATE INDEX contexts_predecessor_idx ON contexts (predecessor);
-- contexts -> successor
CREATE INDEX contexts_successor_idx ON contexts (successor);
