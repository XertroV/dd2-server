-- Add down migration script here

CREATE INDEX session_token_idx ON contexts(session_token);
CREATE INDEX map_id_idx ON contexts(map_id, created_ts);
-- contexts -> predecessor
CREATE INDEX contexts_predecessor_idx ON contexts (predecessor);
-- contexts -> successor
CREATE INDEX contexts_successor_idx ON contexts (successor);
