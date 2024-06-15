-- Add up migration script here
ALTER TABLE colors DROP CONSTRAINT colors_user_id_fkey;
ALTER TABLE colors ADD CONSTRAINT colors_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (web_services_user_id) ON DELETE CASCADE;

ALTER TABLE stats_archive DROP CONSTRAINT stats_archive_user_id_fkey;
ALTER TABLE stats_archive ADD CONSTRAINT stats_archive_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (web_services_user_id) ON DELETE CASCADE;
