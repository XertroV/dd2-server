use serde_json::Value;
use sqlx::{Pool, Postgres, Transaction};
use uuid::Uuid;

use super::CatalogValidationError;
use crate::router::{CollectibleListItem, CollectibleMineItem};

pub struct CatalogCollectible {
    pub slug: String,
    pub name: String,
    pub pickup: String,
    pub collect_on_unlock: bool,
    pub has_zone: bool,
    pub gated_by: Option<String>,
    pub unlock_score: Option<i64>,
}

fn collectible_array(spec: &Value) -> Option<&Vec<Value>> {
    for key in ["collectibles", "collectables"] {
        let Some(v) = spec.get(key) else { continue };
        if let Some(arr) = v.as_array() {
            return Some(arr);
        }
        if let Some(arr) = v.get("items").and_then(|x| x.as_array()) {
            return Some(arr);
        }
        if let Some(arr) = v.get("games").and_then(|x| x.as_array()) {
            return Some(arr);
        }
    }
    None
}

fn parse_collect_paths(item: &Value) -> Option<(bool, bool, &'static str)> {
    let canonical_collect = item.get("collectOnUnlock").and_then(Value::as_bool);
    let canonical_marker = item.get("collectOnUnlock").is_some() || item.get("zone").is_some();
    let has_canonical_zone = item.get("zone").is_some_and(Value::is_object);
    let legacy_pickup = item
        .get("pickup")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase());
    let (legacy_collect, legacy_zone) = match legacy_pickup.as_deref() {
        Some("") | Some("zone") => (false, true),
        Some("grant") => (true, false),
        Some(_) => return None,
        None => (false, !canonical_marker), // intentional legacy default: omitted pickup meant zone
    };
    let collect_on_unlock = canonical_collect.unwrap_or(false) || legacy_collect;
    let has_zone = has_canonical_zone || legacy_zone;
    if !collect_on_unlock && !has_zone {
        return None;
    }
    let compatibility_pickup = if collect_on_unlock { "grant" } else { "zone" };
    Some((collect_on_unlock, has_zone, compatibility_pickup))
}

fn parse_unlock_score(item: &Value) -> Option<i64> {
    item.get("unlock_score").or_else(|| item.get("unlockScore")).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().map(|u| u as i64))
            .or_else(|| v.as_f64().map(|f| f as i64))
    })
}

fn parse_gated_by(item: &Value) -> Option<String> {
    let s = item
        .get("gated_by")
        .or_else(|| item.get("gatedBy"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Replace catalog from `spec.collectibles` / `spec.collectables`.
/// Missing/empty → no rows. Missing slugs get stable sequential defaults;
/// explicit invalid/blank slugs and canonical duplicates reject the spec.
pub fn parse_catalog_collectibles(spec: &Value) -> Result<Vec<CatalogCollectible>, CatalogValidationError> {
    let Some(items) = collectible_array(spec) else {
        return Ok(vec![]);
    };
    let mut parsed = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut next_default = 1_u32;
    for item in items {
        if !item.is_object() {
            continue;
        }
        let Some((collect_on_unlock, has_zone, pickup)) = parse_collect_paths(item) else {
            continue;
        };
        let slug = match item.get("slug") {
            None => format!("Collectible-{next_default}"),
            Some(value) => {
                let Some(slug) = value.as_str() else {
                    return Err(CatalogValidationError::BlankSlug { catalog: "collectible" });
                };
                let slug = slug.trim();
                if slug.is_empty() {
                    return Err(CatalogValidationError::BlankSlug { catalog: "collectible" });
                }
                slug.to_string()
            }
        };
        if let Some(number) = slug.strip_prefix("Collectible-").and_then(|suffix| suffix.parse::<u32>().ok()) {
            if number >= next_default {
                next_default = number.saturating_add(1);
            }
        }
        if !seen.insert(slug.clone()) {
            return Err(CatalogValidationError::DuplicateSlug {
                catalog: "collectible",
                slug,
            });
        }
        let name = item
            .get("name")
            .and_then(|s| s.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&slug)
            .to_string();
        parsed.push(CatalogCollectible {
            slug,
            name,
            pickup: pickup.to_string(),
            collect_on_unlock,
            has_zone,
            gated_by: parse_gated_by(item),
            unlock_score: parse_unlock_score(item),
        });
    }
    Ok(parsed)
}

pub async fn replace_catalog(pool: &Pool<Postgres>, spec_id: i64, user_id: &Uuid, name_id: &str, spec: &Value) -> Result<(), sqlx::Error> {
    let items = parse_catalog_collectibles(spec).map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let mut tx = pool.begin().await?;
    replace_catalog_in_tx(&mut tx, spec_id, user_id, name_id, &items).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn replace_catalog_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    spec_id: i64,
    user_id: &Uuid,
    name_id: &str,
    items: &[CatalogCollectible],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        DELETE FROM collectible_catalog WHERE spec_id = $1
        "#,
        spec_id
    )
    .execute(&mut **tx)
    .await?;
    for c in items {
        sqlx::query!(
            r#"--sql
            INSERT INTO collectible_catalog
                (spec_id, user_id, name_id, slug, name, pickup, collect_on_unlock, has_zone, gated_by, unlock_score)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
            spec_id,
            user_id,
            name_id,
            &c.slug,
            &c.name,
            &c.pickup,
            c.collect_on_unlock,
            c.has_zone,
            c.gated_by.as_deref(),
            c.unlock_score
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub async fn delete_catalog(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        DELETE FROM collectible_catalog WHERE user_id = $1 AND name_id = $2
        "#,
        user_id,
        name_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_catalog(pool: &Pool<Postgres>, spec_id: i64) -> Result<(), sqlx::Error> {
    let existing = sqlx::query!(
        r#"--sql
        SELECT 1 AS ok FROM collectible_catalog WHERE spec_id = $1 LIMIT 1
        "#,
        spec_id
    )
    .fetch_optional(pool)
    .await?;
    if existing.is_some() {
        return Ok(());
    }
    let row = sqlx::query!(
        r#"--sql
        SELECT user_id, name_id, spec FROM custom_map_aux_specs WHERE id = $1
        "#,
        spec_id
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    replace_catalog(pool, spec_id, &row.user_id, &row.name_id, &row.spec).await
}

pub async fn bind_map_from_catalog(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64) -> Result<bool, sqlx::Error> {
    ensure_catalog(pool, spec_id).await?;
    let mut tx = pool.begin().await?;
    let bound = bind_map_from_catalog_in_tx(&mut tx, map_uid, spec_id).await?;
    tx.commit().await?;
    Ok(bound)
}

pub(crate) async fn bind_map_from_catalog_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    map_uid: &str,
    spec_id: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query!("DELETE FROM collectible_on_map WHERE map_uid = $1", map_uid)
        .execute(&mut **tx)
        .await?;
    let r = sqlx::query!(
        r#"--sql
        INSERT INTO collectible_on_map
            (map_uid, slug, collectible_key, name, pickup, collect_on_unlock, has_zone, gated_by, unlock_score, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($3 || ':c:' || c.slug) from 1 for 16),
               c.name, c.pickup, c.collect_on_unlock, c.has_zone, c.gated_by, c.unlock_score, c.user_id, c.name_id
        FROM collectible_catalog c
        WHERE c.spec_id = $2
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            collectible_key = EXCLUDED.collectible_key,
            name = EXCLUDED.name,
            pickup = EXCLUDED.pickup,
            collect_on_unlock = EXCLUDED.collect_on_unlock,
            has_zone = EXCLUDED.has_zone,
            gated_by = EXCLUDED.gated_by,
            unlock_score = EXCLUDED.unlock_score,
            spec_user = EXCLUDED.spec_user,
            name_id = EXCLUDED.name_id
        "#,
        map_uid,
        spec_id,
        map_uid
    )
    .execute(&mut **tx)
    .await?;
    Ok(r.rows_affected() > 0)
}

struct OnMapRow {
    collectible_key: String,
    collect_on_unlock: bool,
    has_zone: bool,
}

async fn lock_materialized_row(
    tx: &mut Transaction<'_, Postgres>,
    map_uid: &str,
    spec_id: i64,
    slug: &str,
) -> Result<Option<OnMapRow>, sqlx::Error> {
    if !crate::queries::listed_maps::lock_materialized_spec(tx, map_uid, spec_id).await? {
        return Ok(None);
    }
    let r = sqlx::query!(
        r#"--sql
        SELECT collectible_key, collect_on_unlock, has_zone
        FROM collectible_on_map
        WHERE map_uid = $1 AND slug = $2
        FOR SHARE
        "#,
        map_uid,
        slug
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(r.map(|row| OnMapRow {
        collectible_key: row.collectible_key,
        collect_on_unlock: row.collect_on_unlock,
        has_zone: row.has_zone,
    }))
}

/// Exact `zone` or `grant` only. Anything else is a silent drop (no progress, no log).
pub(crate) fn collect_log_source(raw: &str) -> Option<&str> {
    match raw {
        "zone" | "grant" => Some(raw),
        _ => None,
    }
}

/// Atomically establish sticky unlock+collection and append the first-collect
/// audit event. Any audit failure rolls the progress transition back.
async fn record_collect_transition(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &Uuid,
    collectible_key: &str,
    map_uid: &str,
    slug: &str,
    pos_x: f64,
    pos_y: f64,
    pos_z: f64,
    source: &str,
) -> Result<(), sqlx::Error> {
    let previous = sqlx::query!(
        r#"--sql
        SELECT collected_at FROM collectible_progress
        WHERE user_id = $1
          AND collectible_key = $2
        FOR UPDATE
        "#,
        user_id,
        collectible_key
    )
    .fetch_optional(&mut **tx)
    .await?;

    sqlx::query!(
        r#"--sql
        INSERT INTO collectible_progress (user_id, collectible_key, map_uid, slug, unlocked_at, collected_at)
        VALUES ($1, $2, $3, $4, now(), now())
        ON CONFLICT (user_id, collectible_key) DO UPDATE SET
            unlocked_at = COALESCE(collectible_progress.unlocked_at, now()),
            collected_at = COALESCE(collectible_progress.collected_at, now())
        "#,
        user_id,
        collectible_key,
        map_uid,
        slug
    )
    .execute(&mut **tx)
    .await?;

    if previous.as_ref().map_or(true, |row| row.collected_at.is_none()) {
        sqlx::query!(
            r#"--sql
            INSERT INTO collectible_collect_log
                (user_id, map_uid, slug, collectible_key, collected_at, pos_x, pos_y, pos_z, source)
            VALUES ($1, $2, $3, $4, now(), $5, $6, $7, $8)
            ON CONFLICT (user_id, collectible_key) DO NOTHING
            "#,
            user_id,
            map_uid,
            slug,
            collectible_key,
            pos_x,
            pos_y,
            pos_z,
            source
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Sticky unlock only. Collection is exclusively report 83 so its audit event
/// always carries the authoritative position.
pub async fn report_unlock(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, slug: &str, spec_id: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(row) = lock_materialized_row(&mut tx, map_uid, spec_id, slug).await? else {
        return Ok(());
    };
    sqlx::query!(
        r#"--sql
        INSERT INTO collectible_progress (user_id, collectible_key, map_uid, slug, unlocked_at, collected_at)
        VALUES ($1, $2, $3, $4, now(), NULL)
        ON CONFLICT (user_id, collectible_key) DO UPDATE SET
            unlocked_at = COALESCE(collectible_progress.unlocked_at, now())
        "#,
        user_id,
        row.collectible_key,
        map_uid,
        slug
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Sticky collect; also unlocks if not already (ungated zone pickup). Unknown slug: silent drop.
/// First collect appends a log row; repeats do not.
pub async fn report_collect(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
    spec_id: i64,
    x: f32,
    y: f32,
    z: f32,
    source: &str,
) -> Result<(), sqlx::Error> {
    let Some(source) = collect_log_source(source) else {
        return Ok(());
    };
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    let Some(row) = lock_materialized_row(&mut tx, map_uid, spec_id, slug).await? else {
        return Ok(());
    };
    let source_allowed = match source {
        "grant" => row.collect_on_unlock,
        "zone" => row.has_zone,
        _ => false,
    };
    if !source_allowed {
        return Ok(());
    }
    record_collect_transition(
        &mut tx,
        user_id,
        &row.collectible_key,
        map_uid,
        slug,
        x as f64,
        y as f64,
        z as f64,
        source,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn get_collectible_list(pool: &Pool<Postgres>, map_uid: &str) -> Result<Vec<CollectibleListItem>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"--sql
        SELECT slug, name, pickup, collect_on_unlock, has_zone, gated_by, unlock_score, collectible_key
        FROM collectible_on_map
        WHERE map_uid = $1
        ORDER BY slug
        "#,
        map_uid
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CollectibleListItem {
            slug: r.slug,
            name: r.name,
            pickup: r.pickup,
            collect_on_unlock: r.collect_on_unlock,
            has_zone: r.has_zone,
            gated_by: r.gated_by,
            unlock_score: r.unlock_score,
            key: r.collectible_key.trim().to_string(),
        })
        .collect())
}

pub async fn get_collectible_mine(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<Vec<CollectibleMineItem>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"--sql
        SELECT m.slug, m.name, m.pickup, m.collect_on_unlock, m.has_zone,
               m.gated_by, m.unlock_score, m.collectible_key,
               p.unlocked_at, p.collected_at
        FROM collectible_on_map m
        LEFT JOIN collectible_progress p
          ON p.collectible_key = m.collectible_key AND p.user_id = $2
        WHERE m.map_uid = $1
        ORDER BY m.slug
        "#,
        map_uid,
        user_id
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CollectibleMineItem {
            slug: r.slug,
            name: r.name,
            pickup: r.pickup,
            collect_on_unlock: r.collect_on_unlock,
            has_zone: r.has_zone,
            gated_by: r.gated_by,
            unlock_score: r.unlock_score,
            key: r.collectible_key.trim().to_string(),
            unlocked_at: r.unlocked_at.map(|t| t.timestamp() as u32),
            collected_at: r.collected_at.map(|t| t.timestamp() as u32),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_is_empty() {
        assert!(parse_catalog_collectibles(&json!({"info":{}})).expect("valid catalog").is_empty());
        assert!(parse_catalog_collectibles(&json!({"collectibles":[]}))
            .expect("valid catalog")
            .is_empty());
        assert!(parse_catalog_collectibles(&json!({"collectables":{"items":[]}}))
            .expect("valid catalog")
            .is_empty());
    }

    #[test]
    fn explicit_blank_slug_invalidates_catalog() {
        let spec = json!({"collectibles":[
            {"name":"a","slug":"","pickup":"zone"},
            {"name":"b","slug":"coin-1","pickup":"nope"}
        ]});
        assert!(matches!(
            parse_catalog_collectibles(&spec),
            Err(CatalogValidationError::BlankSlug { catalog: "collectible" })
        ));
    }

    #[test]
    fn parses_collectibles_and_collectables_alias() {
        let spec = json!({"collectables":[
            {"name":"Coin","slug":"coin-1","pickup":"zone","gatedBy":"TimeTrial-1","unlockScore":12000},
            {"name":"Gift","slug":"gift-1","pickup":"grant"}
        ]});
        let c = parse_catalog_collectibles(&spec).expect("valid catalog");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].slug, "coin-1");
        assert_eq!(c[0].pickup, "zone");
        assert!(!c[0].collect_on_unlock);
        assert!(c[0].has_zone);
        assert_eq!(c[0].gated_by.as_deref(), Some("TimeTrial-1"));
        assert_eq!(c[0].unlock_score, Some(12000));
        assert_eq!(c[1].pickup, "grant");
        assert!(c[1].collect_on_unlock);
        assert!(!c[1].has_zone);
        assert!(c[1].gated_by.is_none());
        assert!(c[1].unlock_score.is_none());
    }

    #[test]
    fn duplicate_canonical_slug_invalidates_catalog() {
        let spec = json!({"collectibles":{"items":[
            {"name":"old","slug":"c-1","pickup":"zone"},
            {"name":"new","slug":"c-1","pickup":"grant","gated_by":"TT-1"}
        ]}});
        assert!(matches!(
            parse_catalog_collectibles(&spec),
            Err(CatalogValidationError::DuplicateSlug {
                catalog: "collectible",
                slug
            }) if slug == "c-1"
        ));
    }

    #[test]
    fn default_pickup_is_zone() {
        let spec = json!({"collectibles":[{"name":"x","slug":"x-1"}]});
        let c = parse_catalog_collectibles(&spec).expect("valid catalog");
        assert_eq!(c[0].pickup, "zone");
        assert!(c[0].has_zone);
        assert!(!c[0].collect_on_unlock);
    }

    #[test]
    fn canonical_editor_json_models_zone_and_collect_on_unlock_independently() {
        let spec = json!({"collectibles":[
            {
                "name":"Both",
                "slug":"both-1",
                "collectOnUnlock":true,
                "zone":{"pos":{"x":1,"y":2,"z":3},"size":{"x":4,"y":5,"z":6}}
            },
            {"name":"Zone","slug":"zone-1","collectOnUnlock":false,"zone":{}},
            {"name":"Grant","slug":"grant-1","collectOnUnlock":true}
        ]});
        let c = parse_catalog_collectibles(&spec).expect("valid catalog");
        assert_eq!(c.len(), 3);
        assert!(c[0].collect_on_unlock && c[0].has_zone);
        assert!(!c[1].collect_on_unlock && c[1].has_zone);
        assert!(c[2].collect_on_unlock && !c[2].has_zone);
    }

    #[test]
    fn canonical_collectible_without_any_collect_path_is_rejected() {
        let spec = json!({"collectibles":[
            {"name":"None","slug":"none-1","collectOnUnlock":false}
        ]});
        assert!(parse_catalog_collectibles(&spec).expect("valid catalog").is_empty());
    }

    #[test]
    fn missing_slugs_default_after_explicit_numbered_reservations() {
        let spec = json!({"collectibles":[
            {"name":"Reserved","slug":"  Collectible-4  ","pickup":"grant"},
            {"name":"Default","pickup":"grant"},
            {"name":"Custom","slug":" custom ","pickup":"grant"},
            {"name":"Next","pickup":"grant"}
        ]});
        let slugs: Vec<_> = parse_catalog_collectibles(&spec)
            .expect("valid catalog")
            .into_iter()
            .map(|item| item.slug)
            .collect();
        assert_eq!(slugs, ["Collectible-4", "Collectible-5", "custom", "Collectible-6"]);
    }

    #[test]
    fn explicit_non_string_slug_invalidates_entire_catalog() {
        let spec = json!({"collectibles":[
            {"slug":"valid","pickup":"grant"},
            {"slug":42,"pickup":"grant"}
        ]});
        assert!(matches!(
            parse_catalog_collectibles(&spec),
            Err(CatalogValidationError::BlankSlug { catalog: "collectible" })
        ));
    }

    #[test]
    fn collect_source_accepts_only_zone_or_grant() {
        assert_eq!(collect_log_source("zone"), Some("zone"));
        assert_eq!(collect_log_source("grant"), Some("grant"));
        assert!(collect_log_source("GRANT").is_none());
        assert!(collect_log_source("  Grant ").is_none());
        assert!(collect_log_source("").is_none());
        assert!(collect_log_source("nope").is_none());
        assert!(collect_log_source("ZONE").is_none());
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Pool<Postgres> {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for collectible tests");
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to DATABASE_URL")
    }

    async fn seed_user(pool: &Pool<Postgres>, name: &str) -> Uuid {
        let user_id = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            name
        )
        .execute(pool)
        .await
        .expect("insert user");
        user_id
    }

    async fn seed_spec(pool: &Pool<Postgres>, user_id: &Uuid) -> (i64, String) {
        let name_id = Uuid::new_v4().simple().to_string();
        let spec = json!({"collectibles":[
            {"name":"Coin","slug":"coin-1","collectOnUnlock":false,"zone":{}},
            {"name":"Gift","slug":"gift-1","collectOnUnlock":true}
        ]});
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec(pool, user_id, &name_id, &spec)
            .await
            .expect("upsert spec");
        replace_catalog(pool, spec_id, user_id, &name_id, &spec)
            .await
            .expect("replace catalog");
        (spec_id, name_id)
    }

    async fn materialize_spec(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64) {
        bind_map_from_catalog(pool, map_uid, spec_id)
            .await
            .expect("bind collectible snapshot");
        sqlx::query!(
            r#"INSERT INTO map_listing_bind
                   (map_uid, spec_id, verified_author_id, author_verified_at,
                    materialized_spec_updated_at, materialized_spec_hash)
               SELECT $1, id, user_id, NOW(), updated_at, md5(spec::text)
               FROM custom_map_aux_specs WHERE id = $2"#,
            map_uid,
            spec_id
        )
        .execute(pool)
        .await
        .expect("mark collectible snapshot materialized");
    }

    fn test_map_uid() -> String {
        let raw = format!("CLog{}", Uuid::new_v4().simple());
        raw.chars().take(27).collect()
    }

    async fn cleanup(pool: &Pool<Postgres>, user_id: &Uuid) {
        let _ = sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(pool)
            .await;
    }

    async fn log_count(pool: &Pool<Postgres>, user_id: &Uuid) -> i64 {
        sqlx::query!("SELECT COUNT(*) AS n FROM collectible_collect_log WHERE user_id = $1", user_id)
            .fetch_one(pool)
            .await
            .expect("count log")
            .n
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn first_zone_collect_logs_once_repeat_does_not() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-zone").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();
        materialize_spec(&pool, &map_uid, spec_id).await;

        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 1.25, 40.5, -3.0, "zone")
            .await
            .expect("first collect");
        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 9.0, 9.0, 9.0, "zone")
            .await
            .expect("repeat collect");

        assert_eq!(log_count(&pool, &user_id).await, 1);
        let row = sqlx::query!(
            r#"SELECT source, pos_x, pos_y, pos_z, slug FROM collectible_collect_log WHERE user_id = $1"#,
            user_id
        )
        .fetch_one(&pool)
        .await
        .expect("log row");
        assert_eq!(row.source, "zone");
        assert_eq!(row.slug, "coin-1");
        assert!((row.pos_x.unwrap() - 1.25).abs() < 1e-9);
        assert!((row.pos_y.unwrap() - 40.5).abs() < 1e-9);
        assert!((row.pos_z.unwrap() - -3.0).abs() < 1e-9);

        let progress = sqlx::query!(
            r#"SELECT collected_at FROM collectible_progress WHERE user_id = $1 AND slug = 'coin-1'"#,
            user_id
        )
        .fetch_one(&pool)
        .await
        .expect("progress");
        assert!(progress.collected_at.is_some());

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn grant_via_report_83_atomically_unlocks_collects_and_logs_position() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-grant").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();
        materialize_spec(&pool, &map_uid, spec_id).await;

        report_unlock(&pool, &user_id, &map_uid, "gift-1", spec_id)
            .await
            .expect("82 is unlock-only");
        assert_eq!(log_count(&pool, &user_id).await, 0);
        let before = sqlx::query!(
            "SELECT unlocked_at, collected_at FROM collectible_progress WHERE user_id = $1 AND slug = 'gift-1'",
            user_id
        )
        .fetch_one(&pool)
        .await
        .expect("unlock progress");
        assert!(before.unlocked_at.is_some());
        assert!(before.collected_at.is_none());

        report_collect(&pool, &user_id, &map_uid, "gift-1", spec_id, 1.0, 2.0, 3.0, "grant")
            .await
            .expect("grant report 83");

        assert_eq!(log_count(&pool, &user_id).await, 1);
        let row = sqlx::query!(
            r#"SELECT source, pos_x, pos_y, pos_z FROM collectible_collect_log WHERE user_id = $1"#,
            user_id
        )
        .fetch_one(&pool)
        .await
        .expect("log row");
        assert_eq!(row.source, "grant");
        assert_eq!(row.pos_x, Some(1.0));
        assert_eq!(row.pos_y, Some(2.0));
        assert_eq!(row.pos_z, Some(3.0));
        let after = sqlx::query!(
            "SELECT unlocked_at, collected_at FROM collectible_progress WHERE user_id = $1 AND slug = 'gift-1'",
            user_id
        )
        .fetch_one(&pool)
        .await
        .expect("collected progress");
        assert!(after.unlocked_at.is_some() && after.collected_at.is_some());

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn unknown_slug_is_silent_and_does_not_log() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-unknown").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();
        materialize_spec(&pool, &map_uid, spec_id).await;

        report_collect(&pool, &user_id, &map_uid, "no-such", spec_id, 0.0, 0.0, 0.0, "zone")
            .await
            .expect("unknown collect");
        report_unlock(&pool, &user_id, &map_uid, "no-such", spec_id)
            .await
            .expect("unknown unlock");

        assert_eq!(log_count(&pool, &user_id).await, 0);
        let n = sqlx::query!("SELECT COUNT(*) AS n FROM collectible_progress WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .expect("count progress")
            .n
            .unwrap_or(0);
        assert_eq!(n, 0);

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn zone_unlock_does_not_log_until_collect() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-unlock-only").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();
        materialize_spec(&pool, &map_uid, spec_id).await;

        report_unlock(&pool, &user_id, &map_uid, "coin-1", spec_id)
            .await
            .expect("zone unlock");
        assert_eq!(log_count(&pool, &user_id).await, 0);

        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 4.0, 5.0, 6.0, "zone")
            .await
            .expect("collect after unlock");
        assert_eq!(log_count(&pool, &user_id).await, 1);

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn invalid_source_is_silent_and_does_not_log() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-bad-source").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();
        materialize_spec(&pool, &map_uid, spec_id).await;

        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 1.0, 2.0, 3.0, "nope")
            .await
            .expect("garbage source");
        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 1.0, 2.0, 3.0, "")
            .await
            .expect("empty source");
        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 1.0, 2.0, 3.0, "ZONE")
            .await
            .expect("wrong-case source");

        assert_eq!(log_count(&pool, &user_id).await, 0);
        let n = sqlx::query!("SELECT COUNT(*) AS n FROM collectible_progress WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .expect("count progress")
            .n
            .unwrap_or(0);
        assert_eq!(n, 0);

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn source_must_match_the_catalog_collect_path() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-wrong-path").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();
        materialize_spec(&pool, &map_uid, spec_id).await;

        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 1.0, 2.0, 3.0, "grant")
            .await
            .expect("invalid grant is dropped");
        report_collect(&pool, &user_id, &map_uid, "gift-1", spec_id, 1.0, 2.0, 3.0, "zone")
            .await
            .expect("invalid zone is dropped");

        assert_eq!(log_count(&pool, &user_id).await, 0);
        let count = sqlx::query!("SELECT COUNT(*) AS n FROM collectible_progress WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .expect("progress count")
            .n
            .unwrap_or(0);
        assert_eq!(count, 0);
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn audit_insert_failure_rolls_back_progress_transition() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-rollback").await;
        let map_uid = test_map_uid();

        let mut tx = pool.begin().await.expect("begin transition");
        let error = record_collect_transition(
            &mut tx,
            &user_id,
            "rollback-key____",
            &map_uid,
            "rollback-1",
            1.0,
            2.0,
            3.0,
            "invalid",
        )
        .await
        .expect_err("audit source constraint must reject the transaction");
        drop(tx);
        assert!(error.to_string().contains("collectible_collect_log_source_check"));
        let progress = sqlx::query!("SELECT COUNT(*) AS n FROM collectible_progress WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .expect("progress count")
            .n
            .unwrap_or(0);
        assert_eq!(progress, 0, "progress must roll back with the audit insert");
        assert_eq!(log_count(&pool, &user_id).await, 0);
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn pre_migration_collected_progress_does_not_get_a_fake_log() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-pre-mig").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();
        materialize_spec(&pool, &map_uid, spec_id).await;

        sqlx::query!(
            r#"--sql
            INSERT INTO collectible_progress (user_id, collectible_key, map_uid, slug, unlocked_at, collected_at)
            VALUES ($1, substring(md5($2 || ':c:' || $3) from 1 for 16), $2, $3, now(), now())
            "#,
            user_id,
            map_uid,
            "coin-1"
        )
        .execute(&pool)
        .await
        .expect("pre-migration progress");
        assert_eq!(log_count(&pool, &user_id).await, 0);

        report_collect(&pool, &user_id, &map_uid, "coin-1", spec_id, 9.0, 8.0, 7.0, "zone")
            .await
            .expect("later 83 must not invent a first-collect log");

        assert_eq!(log_count(&pool, &user_id).await, 0);
        let progress = sqlx::query!(
            r#"SELECT collected_at FROM collectible_progress WHERE user_id = $1 AND slug = 'coin-1'"#,
            user_id
        )
        .fetch_one(&pool)
        .await
        .expect("progress");
        assert!(progress.collected_at.is_some());

        cleanup(&pool, &user_id).await;
    }
}
