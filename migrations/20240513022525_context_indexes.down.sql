-- Add down migration script here
ALTER TABLE contexts DROP COLUMN bi_count;
ALTER TABLE contexts DROP COLUMN editor;
