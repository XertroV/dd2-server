use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{query, query_as, Pool, Postgres};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MapMeta {
    pub map_uid: String,
    pub author_id: Uuid,
    pub map_name: Option<String>,
    pub thumbnail_url: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MapBan {
    pub map_uid: String,
    pub user_id: Uuid,
    pub private_reason: String,
    pub public_message: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub lifted_at: Option<DateTime<Utc>>,
    pub lifted_by: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MapDelegate {
    pub map_uid: String,
    pub user_id: Uuid,
    pub granted_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapBanStatus {
    pub banned: bool,
    pub public_message: Option<String>,
    pub unofficial: bool,
}

pub async fn upsert_map_meta(
    pool: &Pool<Postgres>,
    map_uid: &str,
    author_id: Uuid,
    map_name: Option<&str>,
    thumbnail_url: Option<&str>,
) -> Result<MapMeta, sqlx::Error> {
    query_as!(
        MapMeta,
        r#"
        INSERT INTO map_meta (map_uid, author_id, map_name, thumbnail_url, fetched_at, updated_at)
        VALUES ($1, $2, $3, $4, NOW(), NOW())
        ON CONFLICT (map_uid) DO UPDATE SET
            author_id = EXCLUDED.author_id,
            map_name = EXCLUDED.map_name,
            thumbnail_url = EXCLUDED.thumbnail_url,
            fetched_at = NOW(),
            updated_at = NOW()
        RETURNING map_uid, author_id, map_name, thumbnail_url, fetched_at, updated_at
        "#,
        map_uid,
        author_id,
        map_name,
        thumbnail_url
    )
    .fetch_one(pool)
    .await
}

pub async fn get_map_meta(pool: &Pool<Postgres>, map_uid: &str) -> Result<Option<MapMeta>, sqlx::Error> {
    query_as!(
        MapMeta,
        r#"
        SELECT map_uid, author_id, map_name, thumbnail_url, fetched_at, updated_at
        FROM map_meta WHERE map_uid = $1
        "#,
        map_uid
    )
    .fetch_optional(pool)
    .await
}

pub async fn is_super_mod(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<bool, sqlx::Error> {
    let r = query!(r#"SELECT 1 AS one FROM super_mods WHERE user_id = $1"#, user_id)
        .fetch_optional(pool)
        .await?;
    Ok(r.is_some())
}

pub async fn is_map_delegate(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<bool, sqlx::Error> {
    let r = query!(
        r#"SELECT 1 AS one FROM map_delegates WHERE map_uid = $1 AND user_id = $2"#,
        map_uid,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.is_some())
}

pub async fn is_map_author(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<bool, sqlx::Error> {
    let r = query!(
        r#"SELECT 1 AS one FROM map_meta WHERE map_uid = $1 AND author_id = $2"#,
        map_uid,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.is_some())
}

/// Map Author, Map Delegate, or Super-mod for this Map.
pub async fn can_moderate_map(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<bool, sqlx::Error> {
    if is_super_mod(pool, user_id).await? {
        return Ok(true);
    }
    if is_map_author(pool, map_uid, user_id).await? {
        return Ok(true);
    }
    is_map_delegate(pool, map_uid, user_id).await
}

pub async fn is_protected_from_map_ban(pool: &Pool<Postgres>, map_uid: &str, target: &Uuid) -> Result<bool, sqlx::Error> {
    if is_super_mod(pool, target).await? {
        return Ok(true);
    }
    if is_map_author(pool, map_uid, target).await? {
        return Ok(true);
    }
    is_map_delegate(pool, map_uid, target).await
}

pub async fn list_delegates(pool: &Pool<Postgres>, map_uid: &str) -> Result<Vec<MapDelegate>, sqlx::Error> {
    query_as!(
        MapDelegate,
        r#"
        SELECT d.map_uid, d.user_id, d.granted_by, d.created_at, u.display_name
        FROM map_delegates d
        LEFT JOIN users u ON u.web_services_user_id = d.user_id
        WHERE d.map_uid = $1
        ORDER BY d.created_at
        "#,
        map_uid
    )
    .fetch_all(pool)
    .await
}

pub async fn add_delegate(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid, granted_by: &Uuid) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO map_delegates (map_uid, user_id, granted_by)
        VALUES ($1, $2, $3)
        ON CONFLICT (map_uid, user_id) DO NOTHING
        "#,
        map_uid,
        user_id,
        granted_by
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_delegate(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<(), sqlx::Error> {
    query!(r#"DELETE FROM map_delegates WHERE map_uid = $1 AND user_id = $2"#, map_uid, user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_active_bans(pool: &Pool<Postgres>, map_uid: &str) -> Result<Vec<MapBan>, sqlx::Error> {
    query_as!(
        MapBan,
        r#"
        SELECT map_uid, user_id, private_reason, public_message, created_by, created_at, expires_at, lifted_at, lifted_by
        FROM map_bans
        WHERE map_uid = $1
          AND lifted_at IS NULL
          AND (expires_at IS NULL OR expires_at > NOW())
        ORDER BY created_at DESC
        "#,
        map_uid
    )
    .fetch_all(pool)
    .await
}

pub async fn get_active_ban(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<Option<MapBan>, sqlx::Error> {
    query_as!(
        MapBan,
        r#"
        SELECT map_uid, user_id, private_reason, public_message, created_by, created_at, expires_at, lifted_at, lifted_by
        FROM map_bans
        WHERE map_uid = $1
          AND user_id = $2
          AND lifted_at IS NULL
          AND (expires_at IS NULL OR expires_at > NOW())
        "#,
        map_uid,
        user_id
    )
    .fetch_optional(pool)
    .await
}

pub async fn is_unofficial(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<bool, sqlx::Error> {
    let r = query!(
        r#"SELECT 1 AS one FROM map_unofficial WHERE map_uid = $1 AND user_id = $2"#,
        map_uid,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.is_some())
}

pub async fn get_ban_status(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<MapBanStatus, sqlx::Error> {
    let ban = get_active_ban(pool, map_uid, user_id).await?;
    let unofficial = is_unofficial(pool, map_uid, user_id).await?;
    Ok(MapBanStatus {
        banned: ban.is_some(),
        public_message: ban.map(|b| b.public_message),
        unofficial,
    })
}

pub async fn apply_map_ban(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
    created_by: &Uuid,
    private_reason: &str,
    public_message: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    query!(
        r#"
        INSERT INTO map_bans (map_uid, user_id, private_reason, public_message, created_by, expires_at, lifted_at, lifted_by)
        VALUES ($1, $2, $3, $4, $5, $6, NULL, NULL)
        ON CONFLICT (map_uid, user_id) DO UPDATE SET
            private_reason = EXCLUDED.private_reason,
            public_message = EXCLUDED.public_message,
            created_by = EXCLUDED.created_by,
            created_at = NOW(),
            expires_at = EXCLUDED.expires_at,
            lifted_at = NULL,
            lifted_by = NULL
        "#,
        map_uid,
        user_id,
        private_reason,
        public_message,
        created_by,
        expires_at
    )
    .execute(&mut *tx)
    .await?;

    query!(
        r#"
        INSERT INTO map_unofficial (map_uid, user_id, source, updated_at, updated_by)
        VALUES ($1, $2, 'ban', NOW(), $3)
        ON CONFLICT (map_uid, user_id) DO UPDATE SET
            source = 'ban',
            updated_at = NOW(),
            updated_by = EXCLUDED.updated_by
        "#,
        map_uid,
        user_id,
        created_by
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

pub async fn lift_map_ban(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
    lifted_by: &Uuid,
    keep_unofficial: bool,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let result = query!(
        r#"
        UPDATE map_bans
        SET lifted_at = NOW(), lifted_by = $3
        WHERE map_uid = $1
          AND user_id = $2
          AND lifted_at IS NULL
          AND (expires_at IS NULL OR expires_at > NOW())
        "#,
        map_uid,
        user_id,
        lifted_by
    )
    .execute(&mut *tx)
    .await?;
    let lifted = result.rows_affected() == 1;

    if lifted && !keep_unofficial {
        query!(
            r#"DELETE FROM map_unofficial WHERE map_uid = $1 AND user_id = $2"#,
            map_uid,
            user_id
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(lifted)
}

pub async fn ensure_user(pool: &Pool<Postgres>, user_id: &Uuid, display_name: &str) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO users (web_services_user_id, display_name)
        VALUES ($1, $2)
        ON CONFLICT (web_services_user_id) DO UPDATE SET
            display_name = EXCLUDED.display_name,
            last_login_ts = NOW()
        "#,
        user_id,
        display_name
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Ensure a moderation target satisfies foreign keys without changing an
/// existing user's canonical display name or login timestamp.
pub async fn ensure_user_reference(pool: &Pool<Postgres>, user_id: &Uuid, display_name: Option<&str>) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO users (web_services_user_id, display_name)
        VALUES ($1, $2)
        ON CONFLICT (web_services_user_id) DO NOTHING
        "#,
        user_id,
        display_name.unwrap_or("unknown")
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn add_super_mod(pool: &Pool<Postgres>, user_id: &Uuid, granted_by: &Uuid) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO super_mods (user_id, granted_by)
        VALUES ($1, $2)
        ON CONFLICT (user_id) DO NOTHING
        "#,
        user_id,
        granted_by
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_super_mods(pool: &Pool<Postgres>) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    let rows = query!(
        r#"
        SELECT s.user_id, u.display_name
        FROM super_mods s
        JOIN users u ON u.web_services_user_id = s.user_id
        ORDER BY u.display_name
        "#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.user_id, r.display_name)).collect())
}

pub async fn create_web_session(
    pool: &Pool<Postgres>,
    session_id: &Uuid,
    user_id: &Uuid,
    display_name: &str,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO web_sessions (session_id, user_id, display_name, expires_at)
        VALUES ($1, $2, $3, $4)
        "#,
        session_id,
        user_id,
        display_name,
        expires_at
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_web_session(pool: &Pool<Postgres>, session_id: &Uuid) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    let r = query!(
        r#"
        SELECT user_id, display_name FROM web_sessions
        WHERE session_id = $1 AND expires_at > NOW()
        "#,
        session_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| (row.user_id, row.display_name)))
}

pub async fn delete_web_session(pool: &Pool<Postgres>, session_id: &Uuid) -> Result<(), sqlx::Error> {
    query!(r#"DELETE FROM web_sessions WHERE session_id = $1"#, session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn save_oauth_state(pool: &Pool<Postgres>, state: &str, expires_at: DateTime<Utc>) -> Result<(), sqlx::Error> {
    query!(r#"INSERT INTO oauth_states (state, expires_at) VALUES ($1, $2)"#, state, expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn take_oauth_state(pool: &Pool<Postgres>, state: &str) -> Result<bool, sqlx::Error> {
    let r = query!(
        r#"
        DELETE FROM oauth_states
        WHERE state = $1 AND expires_at > NOW()
        RETURNING state
        "#,
        state
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.is_some())
}

pub async fn find_user_id_by_display_name(pool: &Pool<Postgres>, name: &str) -> Result<Option<Uuid>, sqlx::Error> {
    let r = query!(r#"SELECT web_services_user_id FROM users WHERE display_name = $1 LIMIT 1"#, name)
        .fetch_optional(pool)
        .await?;
    Ok(r.map(|row| row.web_services_user_id))
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AuditEvent {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub actor_id: Uuid,
    pub actor_name: Option<String>,
    pub action: String,
    pub map_uid: Option<String>,
    pub target_user_id: Option<Uuid>,
    pub target_name: Option<String>,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DelegatedMap {
    pub map_uid: String,
    pub author_id: Uuid,
    pub map_name: Option<String>,
    pub thumbnail_url: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub granted_by: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SuperBanRow {
    pub map_uid: String,
    pub map_name: Option<String>,
    pub user_id: Uuid,
    pub display_name: Option<String>,
    pub private_reason: String,
    pub public_message: String,
    pub created_by: Uuid,
    pub created_by_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MapAdminAuthorRow {
    pub author_id: Uuid,
    pub display_name: Option<String>,
    pub map_count: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MapAdminDelegateRow {
    pub map_uid: String,
    pub map_name: Option<String>,
    pub user_id: Uuid,
    pub display_name: Option<String>,
    pub granted_by: Uuid,
    pub granted_by_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuperUserInfo {
    pub user_id: Uuid,
    pub display_name: String,
    pub is_super_mod: bool,
    pub authored_maps: i64,
    pub active_bans: i64,
    pub delegate_seats: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuperModRow {
    pub user_id: Uuid,
    pub display_name: String,
    pub granted_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

pub async fn insert_audit_event(
    pool: &Pool<Postgres>,
    actor_id: &Uuid,
    action: &str,
    map_uid: Option<&str>,
    target_user_id: Option<&Uuid>,
    detail: serde_json::Value,
) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO mod_audit_events (actor_id, action, map_uid, target_user_id, detail)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        actor_id,
        action,
        map_uid,
        target_user_id,
        detail
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_maps_by_author(pool: &Pool<Postgres>, author_id: &Uuid) -> Result<Vec<MapMeta>, sqlx::Error> {
    query_as!(
        MapMeta,
        r#"
        SELECT map_uid, author_id, map_name, thumbnail_url, fetched_at, updated_at
        FROM map_meta
        WHERE author_id = $1
        ORDER BY COALESCE(map_name, map_uid)
        "#,
        author_id
    )
    .fetch_all(pool)
    .await
}

pub async fn list_delegated_maps(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<Vec<DelegatedMap>, sqlx::Error> {
    query_as!(
        DelegatedMap,
        r#"
        SELECT m.map_uid, m.author_id, m.map_name, m.thumbnail_url, m.fetched_at, m.updated_at,
               d.granted_by, d.created_at
        FROM map_delegates d
        JOIN map_meta m ON m.map_uid = d.map_uid
        WHERE d.user_id = $1
        ORDER BY COALESCE(m.map_name, m.map_uid)
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
}

pub async fn search_map_meta(pool: &Pool<Postgres>, q: &str, limit: i64) -> Result<Vec<MapMeta>, sqlx::Error> {
    let pattern = if q.trim().is_empty() {
        "%".to_string()
    } else {
        format!("%{}%", q.trim())
    };
    query_as!(
        MapMeta,
        r#"
        SELECT map_uid, author_id, map_name, thumbnail_url, fetched_at, updated_at
        FROM map_meta
        WHERE map_uid ILIKE $1 OR COALESCE(map_name, '') ILIKE $1 OR author_id::text ILIKE $1
        ORDER BY updated_at DESC
        LIMIT $2
        "#,
        pattern,
        limit
    )
    .fetch_all(pool)
    .await
}

pub async fn list_active_bans_global(pool: &Pool<Postgres>, limit: i64) -> Result<Vec<SuperBanRow>, sqlx::Error> {
    query_as!(
        SuperBanRow,
        r#"
        SELECT b.map_uid,
               m.map_name,
               b.user_id,
               u.display_name,
               b.private_reason,
               b.public_message,
               b.created_by,
               cb.display_name AS created_by_name,
               b.created_at,
               b.expires_at
        FROM map_bans b
        LEFT JOIN map_meta m ON m.map_uid = b.map_uid
        LEFT JOIN users u ON u.web_services_user_id = b.user_id
        LEFT JOIN users cb ON cb.web_services_user_id = b.created_by
        WHERE b.lifted_at IS NULL
          AND (b.expires_at IS NULL OR b.expires_at > NOW())
        ORDER BY b.created_at DESC
        LIMIT $1
        "#,
        limit
    )
    .fetch_all(pool)
    .await
}

pub async fn list_map_admin_authors(pool: &Pool<Postgres>) -> Result<Vec<MapAdminAuthorRow>, sqlx::Error> {
    query_as!(
        MapAdminAuthorRow,
        r#"
        SELECT author_id, display_name, map_count AS "map_count!"
        FROM (
            SELECT m.author_id,
                   u.display_name,
                   COUNT(*)::bigint AS map_count
            FROM map_meta m
            LEFT JOIN users u ON u.web_services_user_id = m.author_id
            GROUP BY m.author_id, u.display_name
        ) sub
        ORDER BY map_count DESC, display_name NULLS LAST
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn list_map_admin_delegates(pool: &Pool<Postgres>) -> Result<Vec<MapAdminDelegateRow>, sqlx::Error> {
    query_as!(
        MapAdminDelegateRow,
        r#"
        SELECT d.map_uid,
               m.map_name,
               d.user_id,
               u.display_name,
               d.granted_by,
               g.display_name AS granted_by_name,
               d.created_at
        FROM map_delegates d
        LEFT JOIN map_meta m ON m.map_uid = d.map_uid
        LEFT JOIN users u ON u.web_services_user_id = d.user_id
        LEFT JOIN users g ON g.web_services_user_id = d.granted_by
        ORDER BY d.created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn list_audit_events(pool: &Pool<Postgres>, limit: i64) -> Result<Vec<AuditEvent>, sqlx::Error> {
    let rows = query!(
        r#"
        SELECT e.id, e.at, e.actor_id,
               COALESCE(a.display_name, '') AS "actor_name!",
               e.action, e.map_uid, e.target_user_id,
               COALESCE(t.display_name, '') AS "target_name!",
               e.detail
        FROM mod_audit_events e
        LEFT JOIN users a ON a.web_services_user_id = e.actor_id
        LEFT JOIN users t ON t.web_services_user_id = e.target_user_id
        ORDER BY e.at DESC
        LIMIT $1
        "#,
        limit
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AuditEvent {
            id: r.id,
            at: r.at,
            actor_id: r.actor_id,
            actor_name: if r.actor_name.is_empty() {
                None
            } else {
                Some(r.actor_name)
            },
            action: r.action,
            map_uid: r.map_uid,
            target_user_id: r.target_user_id,
            target_name: if r.target_name.is_empty() {
                None
            } else {
                Some(r.target_name)
            },
            detail: r.detail,
        })
        .collect())
}

pub async fn get_super_user_info(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<Option<SuperUserInfo>, sqlx::Error> {
    let row = query!(
        r#"
        SELECT u.web_services_user_id AS user_id, u.display_name,
               EXISTS(SELECT 1 FROM super_mods s WHERE s.user_id = u.web_services_user_id) AS "is_super_mod!",
               (SELECT COUNT(*)::bigint FROM map_meta m WHERE m.author_id = u.web_services_user_id) AS "authored_maps!",
               (SELECT COUNT(*)::bigint FROM map_bans b
                 WHERE b.user_id = u.web_services_user_id
                   AND b.lifted_at IS NULL
                   AND (b.expires_at IS NULL OR b.expires_at > NOW())) AS "active_bans!",
               (SELECT COUNT(*)::bigint FROM map_delegates d WHERE d.user_id = u.web_services_user_id) AS "delegate_seats!"
        FROM users u
        WHERE u.web_services_user_id = $1
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| SuperUserInfo {
        user_id: r.user_id,
        display_name: r.display_name,
        is_super_mod: r.is_super_mod,
        authored_maps: r.authored_maps,
        active_bans: r.active_bans,
        delegate_seats: r.delegate_seats,
    }))
}

pub async fn search_users(pool: &Pool<Postgres>, q: &str, limit: i64) -> Result<Vec<SuperUserInfo>, sqlx::Error> {
    let pattern = format!("%{}%", q.trim());
    let rows = query!(
        r#"
        SELECT u.web_services_user_id AS user_id, u.display_name,
               EXISTS(SELECT 1 FROM super_mods s WHERE s.user_id = u.web_services_user_id) AS "is_super_mod!",
               (SELECT COUNT(*)::bigint FROM map_meta m WHERE m.author_id = u.web_services_user_id) AS "authored_maps!",
               (SELECT COUNT(*)::bigint FROM map_bans b
                 WHERE b.user_id = u.web_services_user_id
                   AND b.lifted_at IS NULL
                   AND (b.expires_at IS NULL OR b.expires_at > NOW())) AS "active_bans!",
               (SELECT COUNT(*)::bigint FROM map_delegates d WHERE d.user_id = u.web_services_user_id) AS "delegate_seats!"
        FROM users u
        WHERE u.display_name ILIKE $1 OR u.web_services_user_id::text ILIKE $1
        ORDER BY u.display_name
        LIMIT $2
        "#,
        pattern,
        limit
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SuperUserInfo {
            user_id: r.user_id,
            display_name: r.display_name,
            is_super_mod: r.is_super_mod,
            authored_maps: r.authored_maps,
            active_bans: r.active_bans,
            delegate_seats: r.delegate_seats,
        })
        .collect())
}

pub async fn list_super_mods_detailed(pool: &Pool<Postgres>) -> Result<Vec<SuperModRow>, sqlx::Error> {
    let rows = query!(
        r#"
        SELECT s.user_id, u.display_name, s.granted_by, s.created_at
        FROM super_mods s
        JOIN users u ON u.web_services_user_id = s.user_id
        ORDER BY u.display_name
        "#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SuperModRow {
            user_id: r.user_id,
            display_name: r.display_name,
            granted_by: r.granted_by,
            created_at: r.created_at,
        })
        .collect())
}

pub async fn count_super_mods(pool: &Pool<Postgres>) -> Result<i64, sqlx::Error> {
    let r = query!(r#"SELECT COUNT(*)::bigint AS "count!" FROM super_mods"#)
        .fetch_one(pool)
        .await?;
    Ok(r.count)
}

pub async fn remove_super_mod(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<bool, sqlx::Error> {
    let result = query!(r#"DELETE FROM super_mods WHERE user_id = $1"#, user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Pool<Postgres> {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for moderation tests");
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to DATABASE_URL")
    }

    async fn cleanup(pool: &Pool<Postgres>, map_uid: &str, user_ids: &[Uuid]) {
        for user_id in user_ids {
            let _ = query!("DELETE FROM map_unofficial WHERE map_uid = $1 AND user_id = $2", map_uid, user_id)
                .execute(pool)
                .await;
            let _ = query!("DELETE FROM map_bans WHERE map_uid = $1 AND user_id = $2", map_uid, user_id)
                .execute(pool)
                .await;
            let _ = query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
                .execute(pool)
                .await;
        }
    }

    #[tokio::test]
    async fn expired_ban_cannot_be_lifted_to_clear_unofficial() {
        let pool = test_pool().await;
        let actor = Uuid::now_v7();
        let target = Uuid::now_v7();
        let map_uid = format!("ModerationTest{}", Uuid::new_v4().simple());
        let map_uid = &map_uid[..27];

        ensure_user(&pool, &actor, "moderation-test-actor").await.expect("insert actor");
        ensure_user(&pool, &target, "moderation-test-target").await.expect("insert target");
        apply_map_ban(
            &pool,
            map_uid,
            &target,
            &actor,
            "private",
            "public",
            Some(Utc::now() - chrono::Duration::minutes(1)),
        )
        .await
        .expect("apply expired ban");

        let status = get_ban_status(&pool, map_uid, &target).await.expect("read expired status");
        assert!(!status.banned);
        assert!(status.unofficial);

        let lifted = lift_map_ban(&pool, map_uid, &target, &actor, false).await.expect("attempt lift");
        assert!(!lifted, "an expired ban is not manually liftable");
        assert!(
            is_unofficial(&pool, map_uid, &target).await.expect("read unofficial"),
            "expiry must keep the Unofficial mark"
        );

        cleanup(&pool, map_uid, &[target, actor]).await;
    }

    #[tokio::test]
    async fn manual_unban_can_clear_or_keep_unofficial() {
        let pool = test_pool().await;
        let actor = Uuid::now_v7();
        let target = Uuid::now_v7();
        let map_uid = format!("ManualUnban{}", Uuid::new_v4().simple());
        let map_uid = &map_uid[..27];

        ensure_user(&pool, &actor, "manual-unban-actor").await.expect("insert actor");
        ensure_user(&pool, &target, "manual-unban-target").await.expect("insert target");

        apply_map_ban(&pool, map_uid, &target, &actor, "private", "public", None)
            .await
            .expect("apply first ban");
        assert!(lift_map_ban(&pool, map_uid, &target, &actor, false).await.expect("lift and clear"));
        assert!(!is_unofficial(&pool, map_uid, &target).await.expect("read cleared unofficial"));

        apply_map_ban(&pool, map_uid, &target, &actor, "private", "public", None)
            .await
            .expect("apply second ban");
        assert!(lift_map_ban(&pool, map_uid, &target, &actor, true).await.expect("lift and keep"));
        assert!(is_unofficial(&pool, map_uid, &target).await.expect("read kept unofficial"));

        cleanup(&pool, map_uid, &[target, actor]).await;
    }

    #[tokio::test]
    async fn moderation_reference_does_not_rename_existing_user() {
        let pool = test_pool().await;
        let user_id = Uuid::now_v7();

        ensure_user(&pool, &user_id, "canonical-name").await.expect("insert user");
        ensure_user_reference(&pool, &user_id, None)
            .await
            .expect("ensure existing reference");

        let row = query!("SELECT display_name FROM users WHERE web_services_user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .expect("read user");
        assert_eq!(row.display_name, "canonical-name");

        let _ = query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await;
    }

    #[tokio::test]
    async fn list_maps_by_author_returns_only_own_maps() {
        let pool = test_pool().await;
        let author = Uuid::now_v7();
        let other = Uuid::now_v7();
        let map_a = format!("AuthorMapA{}", Uuid::new_v4().simple());
        let map_b = format!("AuthorMapB{}", Uuid::new_v4().simple());
        let map_a = &map_a[..27];
        let map_b = &map_b[..27];

        ensure_user(&pool, &author, "author-list-a").await.expect("author");
        ensure_user(&pool, &other, "author-list-b").await.expect("other");
        upsert_map_meta(&pool, map_a, author, Some("Alpha"), None)
            .await
            .expect("meta a");
        upsert_map_meta(&pool, map_b, other, Some("Beta"), None)
            .await
            .expect("meta b");

        let mine = list_maps_by_author(&pool, &author).await.expect("list");
        assert!(mine.iter().any(|m| m.map_uid == map_a));
        assert!(!mine.iter().any(|m| m.map_uid == map_b));

        let _ = query!("DELETE FROM map_meta WHERE map_uid = $1 OR map_uid = $2", map_a, map_b)
            .execute(&pool)
            .await;
        let _ = query!("DELETE FROM users WHERE web_services_user_id = $1 OR web_services_user_id = $2", author, other)
            .execute(&pool)
            .await;
    }

    #[tokio::test]
    async fn audit_event_inserted_on_write() {
        let pool = test_pool().await;
        let actor = Uuid::now_v7();
        let target = Uuid::now_v7();
        ensure_user(&pool, &actor, "audit-actor").await.expect("actor");
        ensure_user(&pool, &target, "audit-target").await.expect("target");

        insert_audit_event(
            &pool,
            &actor,
            "ban.create",
            Some("AuditMapUidPlaceholderXX"),
            Some(&target),
            serde_json::json!({"test": true}),
        )
        .await
        .expect("insert audit");

        let events = list_audit_events(&pool, 5).await.expect("list");
        assert!(events.iter().any(|e| e.action == "ban.create" && e.actor_id == actor));

        let _ = query!("DELETE FROM mod_audit_events WHERE actor_id = $1", actor)
            .execute(&pool)
            .await;
        let _ = query!("DELETE FROM users WHERE web_services_user_id = $1 OR web_services_user_id = $2", actor, target)
            .execute(&pool)
            .await;
    }
}
