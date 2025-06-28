use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Pool, Postgres};
use uuid::Uuid;

use crate::api_error::Error;

pub struct CustomMapAuxSpec {
    pub id: i64,
    pub user_id: Uuid,
    pub name_id: String,
    pub spec: Value,
    pub hit_counter: i64, // how many times this aux spec has been accessed
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn get_spec(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str) -> Result<Option<CustomMapAuxSpec>, Error> {
    let spec = sqlx::query_as!(
        CustomMapAuxSpec,
        r#"
        UPDATE custom_map_aux_specs
        SET hit_counter = hit_counter + 1
        WHERE user_id = $1 AND name_id = $2
        RETURNING id, user_id, name_id, spec, created_at, updated_at, hit_counter;
        "#,
        user_id,
        name_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(spec)
}

pub async fn upsert_spec(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str, spec: &Value) -> Result<(), Error> {
    sqlx::query!(
        r#"
        INSERT INTO custom_map_aux_specs (user_id, name_id, spec)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id, name_id)
        DO UPDATE SET spec = $3, updated_at = NOW()
        "#,
        user_id,
        name_id,
        spec
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_spec(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str) -> Result<(), Error> {
    sqlx::query!(
        r#"
        DELETE FROM custom_map_aux_specs
        WHERE user_id = $1 AND name_id = $2
        "#,
        user_id,
        name_id
    )
    .execute(pool)
    .await?;
    Ok(())
}
