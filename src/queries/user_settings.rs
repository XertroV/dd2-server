use sqlx::{pool, query, Pool, Postgres};
use uuid::Uuid;

pub async fn get_profile(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<serde_json::Value, sqlx::Error> {
    let profile = query!(
        r#"--sql
        SELECT s_value FROM profile_settings WHERE user_id = $1
    "#,
        user_id
    )
    .fetch_one(pool)
    .await?
    .s_value;
    Ok(profile)
}

pub async fn set_profile(pool: &Pool<Postgres>, user_id: &Uuid, profile: serde_json::Value) -> Result<(), sqlx::Error> {
    query!(
        r#"--sql
        INSERT INTO profile_settings (user_id, s_value) VALUES ($1, $2)
            ON CONFLICT (user_id) DO UPDATE SET s_value = $2, updated_at = NOW()
    "#,
        user_id,
        profile
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_preferences(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<serde_json::Value, sqlx::Error> {
    let preferences = query!(
        r#"--sql
        SELECT s_value FROM user_preferences WHERE user_id = $1
    "#,
        user_id
    )
    .fetch_one(pool)
    .await?
    .s_value;
    Ok(preferences)
}

pub async fn set_preferences(pool: &Pool<Postgres>, user_id: &Uuid, preferences: serde_json::Value) -> Result<(), sqlx::Error> {
    query!(
        r#"--sql
        INSERT INTO user_preferences (user_id, s_value) VALUES ($1, $2)
            ON CONFLICT (user_id) DO UPDATE SET s_value = $2, updated_at = NOW()
    "#,
        user_id,
        preferences
    )
    .execute(pool)
    .await?;
    Ok(())
}
