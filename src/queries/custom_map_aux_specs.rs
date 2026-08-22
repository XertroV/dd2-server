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

impl Into<Value> for CustomMapAuxSpec {
    fn into(self) -> Value {
        // destructure so we know if anything is missing.
        let CustomMapAuxSpec {
            id,
            user_id,
            name_id,
            spec,
            hit_counter,
            created_at,
            updated_at,
        } = self;
        serde_json::json!({
            "id": id,
            "user_id": user_id.to_string(),
            "name_id": name_id,
            "spec": spec,
            "hit_counter": hit_counter,
            "created_at": created_at.timestamp(),
            "updated_at": updated_at.timestamp(),
        })
    }
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

/// Drop server-injected identity so stored JSON stays mapper-authored.
pub fn strip_injected_spec_id(spec: &Value) -> Value {
    let mut v = spec.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("specId");
        obj.remove("spec_id");
    }
    v
}

/// Play-time GET body: mapper JSON plus `specId` from the row (not stored).
pub fn spec_json_with_id(id: i64, spec: &Value) -> Value {
    let mut v = spec.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("specId".to_string(), Value::from(id));
    }
    v
}

pub async fn upsert_spec(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str, spec: &Value) -> Result<i64, Error> {
    let spec = strip_injected_spec_id(spec);
    let row = sqlx::query!(
        r#"
        INSERT INTO custom_map_aux_specs (user_id, name_id, spec)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id, name_id)
        DO UPDATE SET spec = $3, updated_at = NOW()
        RETURNING id
        "#,
        user_id,
        name_id,
        spec
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id)
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

pub async fn list_specs(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<Vec<CustomMapAuxSpec>, Error> {
    let specs = sqlx::query_as!(
        CustomMapAuxSpec,
        r#"
        SELECT id, user_id, name_id, spec, hit_counter, created_at, updated_at
        FROM custom_map_aux_specs
        WHERE user_id = $1
        ORDER BY created_at DESC;
        "#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_injected_ids_from_stored_json() {
        let spec = json!({"specId": 99, "spec_id": 8, "minigames": {"games": []}});
        let stripped = strip_injected_spec_id(&spec);
        let obj = stripped.as_object().unwrap();
        assert!(!obj.contains_key("specId"));
        assert!(!obj.contains_key("spec_id"));
        assert!(obj.contains_key("minigames"));
    }

    #[test]
    fn get_body_injects_spec_id() {
        let spec = json!({"minigames": {"games": []}});
        let out = spec_json_with_id(42, &spec);
        assert_eq!(out["specId"], 42);
        assert!(out.get("minigames").is_some());
    }
}
