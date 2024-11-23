-- Add up migration script here

-- duplicate index
DROP INDEX session_token_idx;
DROP INDEX map_id_idx;
DROP INDEX contexts_predecessor_idx;
DROP INDEX contexts_successor_idx;
