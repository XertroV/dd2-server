use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Pool, Postgres};
use uuid::Uuid;

use crate::api_error::Error;
use crate::queries::{collectibles, hats, minigames, CatalogValidationError};

#[derive(Debug, thiserror::Error)]
pub enum UpsertWayfinderSpecError {
    #[error(transparent)]
    Validation(#[from] CatalogValidationError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

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
        i64::try_from(u).unwrap_or(MAX_LISTING_THRESHOLD as i64)
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

/// Validate and replace the aux spec plus every derived Wayfinder catalog in
/// one transaction. A validation or database failure leaves the previous spec
/// and all three previous catalogs intact.
pub async fn upsert_spec_and_catalogs(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    name_id: &str,
    spec: &Value,
) -> Result<i64, UpsertWayfinderSpecError> {
    let spec = strip_injected_spec_id(spec);
    let games = minigames::parse_catalog_games(&spec)?;
    let collectibles = collectibles::parse_catalog_collectibles(&spec)?;
    let hats = hats::parse_catalog_hats(&spec)?;
    let listing_threshold = listing_threshold_from_spec(&spec);

    let mut tx = pool.begin().await?;
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
    .fetch_one(&mut *tx)
    .await?;
    minigames::replace_catalog_in_tx(&mut tx, row.id, user_id, name_id, &games).await?;
    collectibles::replace_catalog_in_tx(&mut tx, row.id, user_id, name_id, &collectibles).await?;
    hats::replace_catalog_in_tx(&mut tx, row.id, user_id, name_id, &hats).await?;
    tx.commit().await?;
    Ok(row.id)
}

/// The map bound to this spec, if any. `map_listing_bind.spec_id` is
/// `ON DELETE RESTRICT`, so a bound spec cannot be deleted; callers use this to
/// answer the mapper instead of raising a foreign key violation.
pub async fn bound_map_uid(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str) -> Result<Option<String>, Error> {
    let row = sqlx::query!(
        r#"
        SELECT b.map_uid
        FROM map_listing_bind b
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE s.user_id = $1 AND s.name_id = $2
        ORDER BY b.map_uid
        LIMIT 1
        "#,
        user_id,
        name_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| row.map_uid))
}

/// Owner, name and JSON of one spec row. Shared by the three catalog modules'
/// `ensure_catalog` helpers, which otherwise only differ by table name.
#[cfg(test)]
pub(crate) struct SpecSource {
    pub user_id: Uuid,
    pub name_id: String,
    pub spec: Value,
}

#[cfg(test)]
pub(crate) async fn spec_source(pool: &Pool<Postgres>, spec_id: i64) -> Result<Option<SpecSource>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT user_id, name_id, spec FROM custom_map_aux_specs WHERE id = $1
        "#,
        spec_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| SpecSource {
        user_id: row.user_id,
        name_id: row.name_id,
        spec: row.spec,
    }))
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
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": u64::MAX})), 100);
    }

    #[test]
    fn listing_threshold_truncates_floats_then_clamps() {
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 7.9})), 7);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 4.9})), 5);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 100.9})), 100);
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Pool<Postgres> {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for aux spec tests");
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to DATABASE_URL")
    }

    #[tokio::test]
    async fn aux_spec_and_all_catalogs_roll_back_when_third_catalog_write_fails() {
        let pool = test_pool().await;
        let user_id = Uuid::now_v7();
        let name_id = format!("atomic-{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            "atomic-spec-owner"
        )
        .execute(&pool)
        .await
        .expect("insert user");

        let old_spec = json!({
            "listingThreshold": 12,
            "minigames":{"games":[{"slug":"old-game","kind":"Sprint"}]},
            "collectibles":[{"slug":"old-collectible","pickup":"grant"}],
            "hats":[{"slug":"old-hat","unlock":{"finishThisMap":true}}]
        });
        let spec_id = upsert_spec_and_catalogs(&pool, &user_id, &name_id, &old_spec)
            .await
            .expect("seed atomic spec");

        sqlx::query("DROP TRIGGER IF EXISTS dd2_test_fail_hat_catalog ON hat_catalog")
            .execute(&pool)
            .await
            .expect("drop stale trigger");
        sqlx::query("DROP FUNCTION IF EXISTS dd2_test_fail_hat_catalog()")
            .execute(&pool)
            .await
            .expect("drop stale trigger function");
        sqlx::query(
            "CREATE FUNCTION dd2_test_fail_hat_catalog() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced hat catalog failure'; END $$",
        )
        .execute(&pool)
        .await
        .expect("create trigger function");
        sqlx::query(
            "CREATE TRIGGER dd2_test_fail_hat_catalog BEFORE INSERT ON hat_catalog FOR EACH ROW EXECUTE FUNCTION dd2_test_fail_hat_catalog()",
        )
        .execute(&pool)
        .await
        .expect("create trigger");

        let new_spec = json!({
            "listingThreshold": 88,
            "minigames":{"games":[{"slug":"new-game","kind":"Darts"}]},
            "collectibles":[{"slug":"new-collectible","pickup":"grant"}],
            "hats":[{"slug":"new-hat","unlock":{"finishThisMap":true}}]
        });
        let result = upsert_spec_and_catalogs(&pool, &user_id, &name_id, &new_spec).await;

        sqlx::query("DROP TRIGGER dd2_test_fail_hat_catalog ON hat_catalog")
            .execute(&pool)
            .await
            .expect("drop trigger");
        sqlx::query("DROP FUNCTION dd2_test_fail_hat_catalog()")
            .execute(&pool)
            .await
            .expect("drop trigger function");
        assert!(matches!(result, Err(UpsertWayfinderSpecError::Database(_))));

        let stored = sqlx::query!("SELECT spec, listing_threshold FROM custom_map_aux_specs WHERE id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("read rolled back spec");
        assert_eq!(stored.spec, old_spec);
        assert_eq!(stored.listing_threshold, 12);
        let game = sqlx::query!("SELECT slug FROM minigame_catalog WHERE spec_id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("old minigame retained");
        let collectible = sqlx::query!("SELECT slug FROM collectible_catalog WHERE spec_id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("old collectible retained");
        let hat = sqlx::query!("SELECT slug FROM hat_catalog WHERE spec_id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("old hat retained");
        assert_eq!(game.slug, "old-game");
        assert_eq!(collectible.slug, "old-collectible");
        assert_eq!(hat.slug, "old-hat");

        sqlx::query!("DELETE FROM custom_map_aux_specs WHERE id = $1", spec_id)
            .execute(&pool)
            .await
            .expect("delete spec");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("delete user");
    }
}
