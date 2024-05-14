-- Add up migration script here
-- CREATE INDEX CONCURRENTLY context_big_ix ON contexts(created_ts, has_vl_item, map_id, flags_raw, managers);
-- CREATE INDEX CONCURRENTLY ctx_session_item_created_idx ON contexts(session_token, has_vl_item, flags_raw, created_ts DESC);
ALTER TABLE contexts ADD COLUMN bi_count INTEGER NOT NULL DEFAULT -1;
ALTER TABLE contexts ADD COLUMN editor BOOLEAN NULL;
