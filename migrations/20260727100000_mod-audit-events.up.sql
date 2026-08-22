-- Append-only moderation audit trail (ban / delegate / super-mod mutations)
CREATE TABLE mod_audit_events (
    id BIGSERIAL PRIMARY KEY,
    at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    actor_id UUID NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    action TEXT NOT NULL,
    map_uid VARCHAR(27),
    target_user_id UUID REFERENCES users (web_services_user_id) ON DELETE SET NULL,
    detail JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX mod_audit_events_at_idx ON mod_audit_events (at DESC);
CREATE INDEX mod_audit_events_map_uid_idx ON mod_audit_events (map_uid)
    WHERE map_uid IS NOT NULL;
CREATE INDEX mod_audit_events_actor_id_idx ON mod_audit_events (actor_id);
