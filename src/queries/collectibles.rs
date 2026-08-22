use serde_json::Value;
use sqlx::{Pool, Postgres};
use uuid::Uuid;

use crate::router::{CollectibleListItem, CollectibleMineItem};

pub struct CatalogCollectible {
    pub slug: String,
    pub name: String,
    pub pickup: String,
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

fn parse_pickup(item: &Value) -> Option<&'static str> {
    let raw = item
        .get("pickup")
        .and_then(|v| v.as_str())
        .unwrap_or("zone")
        .trim();
    match raw.to_ascii_lowercase().as_str() {
        "" | "zone" => Some("zone"),
        "grant" => Some("grant"),
        _ => None,
    }
}

fn parse_unlock_score(item: &Value) -> Option<i64> {
    item.get("unlock_score")
        .or_else(|| item.get("unlockScore"))
        .and_then(|v| {
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
/// Missing/empty → no rows. Empty slug skipped. Duplicate slug: last wins.
pub fn parse_catalog_collectibles(spec: &Value) -> Vec<CatalogCollectible> {
    let Some(items) = collectible_array(spec) else {
        return vec![];
    };
    let mut last_by_slug = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for item in items {
        if !item.is_object() {
            continue;
        }
        let Some(pickup) = parse_pickup(item) else {
            continue;
        };
        let slug = item.get("slug").and_then(|s| s.as_str()).unwrap_or("").trim();
        if slug.is_empty() {
            continue;
        }
        let slug = slug.to_string();
        let name = item.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if !last_by_slug.contains_key(&slug) {
            order.push(slug.clone());
        }
        last_by_slug.insert(
            slug.clone(),
            CatalogCollectible {
                slug,
                name,
                pickup: pickup.to_string(),
                gated_by: parse_gated_by(item),
                unlock_score: parse_unlock_score(item),
            },
        );
    }
    order
        .into_iter()
        .filter_map(|s| last_by_slug.remove(&s))
        .collect()
}

pub async fn replace_catalog(
    pool: &Pool<Postgres>,
    spec_id: i64,
    user_id: &Uuid,
    name_id: &str,
    spec: &Value,
) -> Result<(), sqlx::Error> {
    let items = parse_catalog_collectibles(spec);
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"--sql
        DELETE FROM collectible_catalog WHERE spec_id = $1
        "#,
        spec_id
    )
    .execute(&mut *tx)
    .await?;
    for c in items {
        sqlx::query!(
            r#"--sql
            INSERT INTO collectible_catalog (spec_id, user_id, name_id, slug, name, pickup, gated_by, unlock_score)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
            spec_id,
            user_id,
            name_id,
            &c.slug,
            &c.name,
            &c.pickup,
            c.gated_by.as_deref(),
            c.unlock_score
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
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
    let r = sqlx::query!(
        r#"--sql
        INSERT INTO collectible_on_map (map_uid, slug, collectible_key, name, pickup, gated_by, unlock_score, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($3 || ':c:' || c.slug) from 1 for 16), c.name, c.pickup, c.gated_by, c.unlock_score, c.user_id, c.name_id
        FROM collectible_catalog c
        WHERE c.spec_id = $2
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            collectible_key = EXCLUDED.collectible_key,
            name = EXCLUDED.name,
            pickup = EXCLUDED.pickup,
            gated_by = EXCLUDED.gated_by,
            unlock_score = EXCLUDED.unlock_score,
            spec_user = EXCLUDED.spec_user,
            name_id = EXCLUDED.name_id
        "#,
        map_uid,
        spec_id,
        map_uid
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

struct CatalogRow {
    pickup: String,
}

async fn catalog_row(pool: &Pool<Postgres>, spec_id: i64, slug: &str) -> Result<Option<CatalogRow>, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT pickup FROM collectible_catalog WHERE spec_id = $1 AND slug = $2
        "#,
        spec_id,
        slug
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| CatalogRow { pickup: row.pickup }))
}

async fn bind_one(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64, slug: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        INSERT INTO collectible_on_map (map_uid, slug, collectible_key, name, pickup, gated_by, unlock_score, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($4 || ':c:' || c.slug) from 1 for 16), c.name, c.pickup, c.gated_by, c.unlock_score, c.user_id, c.name_id
        FROM collectible_catalog c
        WHERE c.spec_id = $2 AND c.slug = $3
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            collectible_key = EXCLUDED.collectible_key,
            name = EXCLUDED.name,
            pickup = EXCLUDED.pickup,
            gated_by = EXCLUDED.gated_by,
            unlock_score = EXCLUDED.unlock_score,
            spec_user = EXCLUDED.spec_user,
            name_id = EXCLUDED.name_id
        "#,
        map_uid,
        spec_id,
        slug,
        map_uid
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn collect_log_source(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "grant" => "grant",
        _ => "zone",
    }
}

/// First accepted collect only. Progress upsert must run after this so a prior
/// `collected_at` still suppresses the log (including pre-migration collects).
async fn insert_first_collect_log(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
    pos_x: Option<f64>,
    pos_y: Option<f64>,
    pos_z: Option<f64>,
    source: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        INSERT INTO collectible_collect_log (user_id, map_uid, slug, collectible_key, collected_at, pos_x, pos_y, pos_z, source)
        SELECT $1, $2, $3, substring(md5($4 || ':c:' || $3) from 1 for 16), now(), $5, $6, $7, $8
        WHERE NOT EXISTS (
            SELECT 1 FROM collectible_progress
            WHERE user_id = $1
              AND collectible_key = substring(md5($4 || ':c:' || $3) from 1 for 16)
              AND collected_at IS NOT NULL
        )
        ON CONFLICT (user_id, collectible_key) DO NOTHING
        "#,
        user_id,
        map_uid,
        slug,
        map_uid,
        pos_x,
        pos_y,
        pos_z,
        source
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Sticky unlock. Grant pickup also collects. Unknown slug: silent drop.
pub async fn report_unlock(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
    spec_id: i64,
) -> Result<(), sqlx::Error> {
    ensure_catalog(pool, spec_id).await?;
    let Some(row) = catalog_row(pool, spec_id, slug).await? else {
        return Ok(());
    };
    bind_one(pool, map_uid, spec_id, slug).await?;
    let grant = row.pickup == "grant";
    if grant {
        insert_first_collect_log(pool, user_id, map_uid, slug, None, None, None, "grant").await?;
    }
    sqlx::query!(
        r#"--sql
        INSERT INTO collectible_progress (user_id, collectible_key, map_uid, slug, unlocked_at, collected_at)
        VALUES ($1, substring(md5($2 || ':c:' || $3) from 1 for 16), $2, $3, now(), CASE WHEN $4 THEN now() ELSE NULL END)
        ON CONFLICT (user_id, collectible_key) DO UPDATE SET
            unlocked_at = COALESCE(collectible_progress.unlocked_at, now()),
            collected_at = CASE WHEN $4 THEN COALESCE(collectible_progress.collected_at, now()) ELSE collectible_progress.collected_at END
        "#,
        user_id,
        map_uid,
        slug,
        grant
    )
    .execute(pool)
    .await?;
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
    ensure_catalog(pool, spec_id).await?;
    if catalog_row(pool, spec_id, slug).await?.is_none() {
        return Ok(());
    }
    bind_one(pool, map_uid, spec_id, slug).await?;
    insert_first_collect_log(
        pool,
        user_id,
        map_uid,
        slug,
        Some(x as f64),
        Some(y as f64),
        Some(z as f64),
        collect_log_source(source),
    )
    .await?;
    sqlx::query!(
        r#"--sql
        INSERT INTO collectible_progress (user_id, collectible_key, map_uid, slug, unlocked_at, collected_at)
        VALUES ($1, substring(md5($2 || ':c:' || $3) from 1 for 16), $2, $3, now(), now())
        ON CONFLICT (user_id, collectible_key) DO UPDATE SET
            unlocked_at = COALESCE(collectible_progress.unlocked_at, now()),
            collected_at = COALESCE(collectible_progress.collected_at, now())
        "#,
        user_id,
        map_uid,
        slug
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_collectible_list(pool: &Pool<Postgres>, map_uid: &str) -> Result<Vec<CollectibleListItem>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"--sql
        SELECT slug, name, pickup, gated_by, unlock_score, collectible_key
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
            gated_by: r.gated_by,
            unlock_score: r.unlock_score,
            key: r.collectible_key.trim().to_string(),
        })
        .collect())
}

pub async fn get_collectible_mine(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
) -> Result<Vec<CollectibleMineItem>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"--sql
        SELECT m.slug, m.name, m.pickup, m.gated_by, m.unlock_score, m.collectible_key,
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
        assert!(parse_catalog_collectibles(&json!({"info":{}})).is_empty());
        assert!(parse_catalog_collectibles(&json!({"collectibles":[]})).is_empty());
        assert!(parse_catalog_collectibles(&json!({"collectables":{"items":[]}})).is_empty());
    }

    #[test]
    fn skips_empty_slug_and_unknown_pickup() {
        let spec = json!({"collectibles":[
            {"name":"a","slug":"","pickup":"zone"},
            {"name":"b","slug":"coin-1","pickup":"nope"}
        ]});
        assert!(parse_catalog_collectibles(&spec).is_empty());
    }

    #[test]
    fn parses_collectibles_and_collectables_alias() {
        let spec = json!({"collectables":[
            {"name":"Coin","slug":"coin-1","pickup":"zone","gatedBy":"TimeTrial-1","unlockScore":12000},
            {"name":"Gift","slug":"gift-1","pickup":"grant"}
        ]});
        let c = parse_catalog_collectibles(&spec);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].slug, "coin-1");
        assert_eq!(c[0].pickup, "zone");
        assert_eq!(c[0].gated_by.as_deref(), Some("TimeTrial-1"));
        assert_eq!(c[0].unlock_score, Some(12000));
        assert_eq!(c[1].pickup, "grant");
        assert!(c[1].gated_by.is_none());
        assert!(c[1].unlock_score.is_none());
    }

    #[test]
    fn last_duplicate_slug_wins() {
        let spec = json!({"collectibles":{"items":[
            {"name":"old","slug":"c-1","pickup":"zone"},
            {"name":"new","slug":"c-1","pickup":"grant","gated_by":"TT-1"}
        ]}});
        let c = parse_catalog_collectibles(&spec);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "new");
        assert_eq!(c[0].pickup, "grant");
        assert_eq!(c[0].gated_by.as_deref(), Some("TT-1"));
    }

    #[test]
    fn default_pickup_is_zone() {
        let spec = json!({"collectibles":[{"name":"x","slug":"x-1"}]});
        let c = parse_catalog_collectibles(&spec);
        assert_eq!(c[0].pickup, "zone");
    }

    #[test]
    fn collect_source_defaults_unknown_to_zone() {
        assert_eq!(collect_log_source("zone"), "zone");
        assert_eq!(collect_log_source("GRANT"), "grant");
        assert_eq!(collect_log_source("  Grant "), "grant");
        assert_eq!(collect_log_source(""), "zone");
        assert_eq!(collect_log_source("nope"), "zone");
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
            {"name":"Coin","slug":"coin-1","pickup":"zone"},
            {"name":"Gift","slug":"gift-1","pickup":"grant"}
        ]});
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec(pool, user_id, &name_id, &spec)
            .await
            .expect("upsert spec");
        replace_catalog(pool, spec_id, user_id, &name_id, &spec)
            .await
            .expect("replace catalog");
        (spec_id, name_id)
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
        sqlx::query!(
            "SELECT COUNT(*) AS n FROM collectible_collect_log WHERE user_id = $1",
            user_id
        )
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
    async fn grant_via_unlock_logs_once_with_null_pos() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-grant").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();

        report_unlock(&pool, &user_id, &map_uid, "gift-1", spec_id)
            .await
            .expect("grant unlock");
        report_unlock(&pool, &user_id, &map_uid, "gift-1", spec_id)
            .await
            .expect("repeat grant");
        report_collect(&pool, &user_id, &map_uid, "gift-1", spec_id, 1.0, 2.0, 3.0, "zone")
            .await
            .expect("later 83 must not log again");

        assert_eq!(log_count(&pool, &user_id).await, 1);
        let row = sqlx::query!(
            r#"SELECT source, pos_x, pos_y, pos_z FROM collectible_collect_log WHERE user_id = $1"#,
            user_id
        )
        .fetch_one(&pool)
        .await
        .expect("log row");
        assert_eq!(row.source, "grant");
        assert!(row.pos_x.is_none() && row.pos_y.is_none() && row.pos_z.is_none());

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn unknown_slug_is_silent_and_does_not_log() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "clog-unknown").await;
        let (spec_id, _) = seed_spec(&pool, &user_id).await;
        let map_uid = test_map_uid();

        report_collect(&pool, &user_id, &map_uid, "no-such", spec_id, 0.0, 0.0, 0.0, "zone")
            .await
            .expect("unknown collect");
        report_unlock(&pool, &user_id, &map_uid, "no-such", spec_id)
            .await
            .expect("unknown unlock");

        assert_eq!(log_count(&pool, &user_id).await, 0);
        let n = sqlx::query!(
            "SELECT COUNT(*) AS n FROM collectible_progress WHERE user_id = $1",
            user_id
        )
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
}
