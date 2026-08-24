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

pub const DEFAULT_LISTING_THRESHOLD: i32 = 25;
pub const MIN_LISTING_THRESHOLD: i32 = 5;
pub const MAX_LISTING_THRESHOLD: i32 = 100;

/// Top-level aux spec `listingThreshold`: omit/invalid = 25, clamp 5–100.
pub fn listing_threshold_from_spec(spec: &Value) -> i32 {
    let Some(raw) = spec.get("listingThreshold") else {
        return DEFAULT_LISTING_THRESHOLD;
    };
    let n = if let Some(i) = raw.as_i64() {
        i
    } else if let Some(u) = raw.as_u64() {
        i64::try_from(u).unwrap_or(DEFAULT_LISTING_THRESHOLD as i64)
    } else if let Some(f) = raw.as_f64() {
        if !f.is_finite() {
            return DEFAULT_LISTING_THRESHOLD;
        }
        f.trunc() as i64
    } else {
        return DEFAULT_LISTING_THRESHOLD;
    };
    n.clamp(MIN_LISTING_THRESHOLD as i64, MAX_LISTING_THRESHOLD as i64) as i32
}

pub async fn upsert_spec(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str, spec: &Value) -> Result<i64, Error> {
    let spec = strip_injected_spec_id(spec);
    let listing_threshold = listing_threshold_from_spec(&spec);
    let row = sqlx::query!(
        r#"
        INSERT INTO custom_map_aux_specs (user_id, name_id, spec, listing_threshold)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (user_id, name_id)
        DO UPDATE SET spec = $3, listing_threshold = $4, updated_at = NOW()
        RETURNING id
        "#,
        user_id,
        name_id,
        spec,
        listing_threshold
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

    #[test]
    fn listing_threshold_omitted_defaults_to_25() {
        assert_eq!(listing_threshold_from_spec(&json!({})), 25);
        assert_eq!(listing_threshold_from_spec(&json!({"minigames": {}})), 25);
    }

    #[test]
    fn listing_threshold_null_or_invalid_defaults_to_25() {
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": null})), 25);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": "25"})), 25);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": true})), 25);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": []})), 25);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": {}})), 25);
    }

    #[test]
    fn listing_threshold_keeps_in_range_integers() {
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 5})), 5);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 25})), 25);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 100})), 100);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 42})), 42);
    }

    #[test]
    fn listing_threshold_clamps_to_5_100() {
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 4})), 5);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 0})), 5);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": -10})), 5);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 101})), 100);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 1000})), 100);
    }

    #[test]
    fn listing_threshold_truncates_floats_then_clamps() {
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 7.9})), 7);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 4.9})), 5);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 100.9})), 100);
    }
}
