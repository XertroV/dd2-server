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
    let r = query!(
        r#"SELECT 1 AS one FROM super_mods WHERE user_id = $1"#,
        user_id
    )
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

pub async fn add_delegate(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
    granted_by: &Uuid,
) -> Result<(), sqlx::Error> {
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
    query!(
        r#"DELETE FROM map_delegates WHERE map_uid = $1 AND user_id = $2"#,
        map_uid,
        user_id
    )
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

pub async fn get_active_ban(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
) -> Result<Option<MapBan>, sqlx::Error> {
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

pub async fn get_ban_status(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
) -> Result<MapBanStatus, sqlx::Error> {
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
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    query!(
        r#"
        UPDATE map_bans
        SET lifted_at = NOW(), lifted_by = $3
        WHERE map_uid = $1 AND user_id = $2 AND lifted_at IS NULL
        "#,
        map_uid,
        user_id,
        lifted_by
    )
    .execute(&mut *tx)
    .await?;

    if !keep_unofficial {
        query!(
            r#"DELETE FROM map_unofficial WHERE map_uid = $1 AND user_id = $2"#,
            map_uid,
            user_id
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
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
    access_token: Option<&str>,
    refresh_token: Option<&str>,
) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO web_sessions (session_id, user_id, display_name, expires_at, oauth_access_token, oauth_refresh_token)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        session_id,
        user_id,
        display_name,
        expires_at,
        access_token,
        refresh_token
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_web_session(
    pool: &Pool<Postgres>,
    session_id: &Uuid,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
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
    query!(
        r#"INSERT INTO oauth_states (state, expires_at) VALUES ($1, $2)"#,
        state,
        expires_at
    )
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
    let r = query!(
        r#"SELECT web_services_user_id FROM users WHERE display_name = $1 LIMIT 1"#,
        name
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| row.web_services_user_id))
}
