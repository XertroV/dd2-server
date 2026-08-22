use serde_json::{json, Value};
use sqlx::{Pool, Postgres};
use uuid::Uuid;

use crate::consts::DD2_MAP_UID;
use crate::router::{HatMineItem, HatOnMapPlayer};

/// Official DD2 finish-floor Y. Used only as a height analog when `extra.finish` is absent.
const DD2_FINISH_HEIGHT: f64 = 1910.0;

pub struct CatalogHat {
    pub slug: String,
    pub name: String,
    pub finish_this_map: bool,
    pub record_uids: Vec<String>,
}

fn hats_array(spec: &Value) -> Option<&Vec<Value>> {
    let v = spec.get("hats")?;
    if let Some(arr) = v.as_array() {
        return Some(arr);
    }
    if let Some(arr) = v.get("items").and_then(|x| x.as_array()) {
        return Some(arr);
    }
    None
}

fn parse_unlock(item: &Value) -> (bool, Vec<String>) {
    let Some(unlock) = item.get("unlock") else {
        return (false, vec![]);
    };
    let finish = unlock
        .get("finishThisMap")
        .or_else(|| unlock.get("finish_this_map"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let raw = unlock
        .get("recordUids")
        .or_else(|| unlock.get("record_uids"))
        .and_then(|v| v.as_array());
    let mut uids = Vec::new();
    if let Some(arr) = raw {
        for v in arr {
            let Some(s) = v.as_str() else { continue };
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            uids.push(s.to_string());
        }
    }
    (finish, uids)
}

fn uids_from_json(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Replace catalog from `spec.hats[]`. Missing/empty → no rows. Empty slug skipped. Duplicate slug: last wins.
pub fn parse_catalog_hats(spec: &Value) -> Vec<CatalogHat> {
    let Some(items) = hats_array(spec) else {
        return vec![];
    };
    let mut last_by_slug = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for item in items {
        if !item.is_object() {
            continue;
        }
        let slug = item.get("slug").and_then(|s| s.as_str()).unwrap_or("").trim();
        if slug.is_empty() {
            continue;
        }
        let slug = slug.to_string();
        let name = item.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let (finish_this_map, record_uids) = parse_unlock(item);
        if !last_by_slug.contains_key(&slug) {
            order.push(slug.clone());
        }
        last_by_slug.insert(
            slug.clone(),
            CatalogHat {
                slug,
                name,
                finish_this_map,
                record_uids,
            },
        );
    }
    order.into_iter().filter_map(|s| last_by_slug.remove(&s)).collect()
}

pub async fn replace_catalog(
    pool: &Pool<Postgres>,
    spec_id: i64,
    user_id: &Uuid,
    name_id: &str,
    spec: &Value,
) -> Result<(), sqlx::Error> {
    let items = parse_catalog_hats(spec);
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"--sql
        DELETE FROM hat_catalog WHERE spec_id = $1
        "#,
        spec_id
    )
    .execute(&mut *tx)
    .await?;
    for h in items {
        let record_uids = json!(h.record_uids);
        sqlx::query!(
            r#"--sql
            INSERT INTO hat_catalog (spec_id, user_id, name_id, slug, name, finish_this_map, record_uids)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
            spec_id,
            user_id,
            name_id,
            &h.slug,
            &h.name,
            h.finish_this_map,
            record_uids
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
        DELETE FROM hat_catalog WHERE user_id = $1 AND name_id = $2
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
        SELECT 1 AS ok FROM hat_catalog WHERE spec_id = $1 LIMIT 1
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
        INSERT INTO hat_on_map (map_uid, slug, hat_key, name, finish_this_map, record_uids, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($3 || ':h:' || c.slug) from 1 for 16), c.name, c.finish_this_map, c.record_uids, c.user_id, c.name_id
        FROM hat_catalog c
        WHERE c.spec_id = $2
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            hat_key = EXCLUDED.hat_key,
            name = EXCLUDED.name,
            finish_this_map = EXCLUDED.finish_this_map,
            record_uids = EXCLUDED.record_uids,
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
    finish_this_map: bool,
    record_uids: Vec<String>,
}

async fn catalog_row(pool: &Pool<Postgres>, spec_id: i64, slug: &str) -> Result<Option<CatalogRow>, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT finish_this_map, record_uids FROM hat_catalog WHERE spec_id = $1 AND slug = $2
        "#,
        spec_id,
        slug
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| CatalogRow {
        finish_this_map: row.finish_this_map,
        record_uids: uids_from_json(&row.record_uids),
    }))
}

async fn bind_one(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64, slug: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        INSERT INTO hat_on_map (map_uid, slug, hat_key, name, finish_this_map, record_uids, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($4 || ':h:' || c.slug) from 1 for 16), c.name, c.finish_this_map, c.record_uids, c.user_id, c.name_id
        FROM hat_catalog c
        WHERE c.spec_id = $2 AND c.slug = $3
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            hat_key = EXCLUDED.hat_key,
            name = EXCLUDED.name,
            finish_this_map = EXCLUDED.finish_this_map,
            record_uids = EXCLUDED.record_uids,
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

fn json_finish_count(extra: Option<&Value>) -> i64 {
    extra
        .and_then(|e| e.get("finish"))
        .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)).or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

/// Live / stored finish analog for this map. No ban filter. Fail closed if unprovable.
///
/// Custom maps: `custom_map_stats.stats.extra.finish > 0` (race-finish counter already stored).
/// Height is not used for custom maps: the server has no finish-floor threshold.
/// Official DD2: `stats.extra.finish > 0` or global `leaderboard.height >= 1910`.
pub async fn validate_finish_this_map(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str) -> Result<bool, sqlx::Error> {
    if map_uid == DD2_MAP_UID {
        let stats = sqlx::query!(
            r#"--sql
            SELECT extra FROM stats WHERE user_id = $1
            "#,
            user_id
        )
        .fetch_optional(pool)
        .await?;
        if json_finish_count(stats.as_ref().map(|r| &r.extra)) > 0 {
            return Ok(true);
        }
        let lb = sqlx::query!(
            r#"--sql
            SELECT height FROM leaderboard WHERE user_id = $1
            "#,
            user_id
        )
        .fetch_optional(pool)
        .await?;
        return Ok(lb.map(|r| r.height >= DD2_FINISH_HEIGHT).unwrap_or(false));
    }

    let row = sqlx::query!(
        r#"--sql
        SELECT stats FROM custom_map_stats WHERE user_id = $1 AND map_uid = $2
        "#,
        user_id,
        map_uid
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    Ok(json_finish_count(row.stats.get("extra")) > 0)
}

/// Official-record analog for a named UID. No Nadeo player-record API exists in this server:
/// unknown UIDs with no local row fail closed. No ban filter.
///
/// Custom maps: a `map_leaderboard` row (height / race_time analog).
/// Official DD2: a global `leaderboard` row.
pub async fn validate_record_for_uid(pool: &Pool<Postgres>, user_id: &Uuid, uid: &str) -> Result<bool, sqlx::Error> {
    if uid == DD2_MAP_UID {
        let lb = sqlx::query!(
            r#"--sql
            SELECT 1 AS ok FROM leaderboard WHERE user_id = $1
            "#,
            user_id
        )
        .fetch_optional(pool)
        .await?;
        return Ok(lb.is_some());
    }
    let row = sqlx::query!(
        r#"--sql
        SELECT 1 AS ok FROM map_leaderboard WHERE user_id = $1 AND map_uid = $2
        "#,
        user_id,
        uid
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

async fn validate_hat_unlock(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    row: &CatalogRow,
) -> Result<bool, sqlx::Error> {
    if !row.finish_this_map && row.record_uids.is_empty() {
        // Nothing the server can prove.
        return Ok(false);
    }
    if row.finish_this_map && !validate_finish_this_map(pool, user_id, map_uid).await? {
        return Ok(false);
    }
    for uid in &row.record_uids {
        if !validate_record_for_uid(pool, user_id, uid).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn already_unlocked(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, slug: &str) -> Result<bool, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT 1 AS ok FROM hat_unlock
        WHERE user_id = $1 AND hat_key = substring(md5($2 || ':h:' || $3) from 1 for 16)
        "#,
        user_id,
        map_uid,
        slug
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.is_some())
}

/// Sticky unlock. Unknown slug / failed validation: silent drop. Bans are ignored.
pub async fn report_unlock(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
    spec_id: i64,
) -> Result<bool, sqlx::Error> {
    ensure_catalog(pool, spec_id).await?;
    let Some(row) = catalog_row(pool, spec_id, slug).await? else {
        return Ok(false);
    };
    bind_one(pool, map_uid, spec_id, slug).await?;
    if already_unlocked(pool, user_id, map_uid, slug).await? {
        return Ok(true);
    }
    if !validate_hat_unlock(pool, user_id, map_uid, &row).await? {
        return Ok(false);
    }
    sqlx::query!(
        r#"--sql
        INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug, unlocked_at)
        VALUES ($1, substring(md5($4 || ':h:' || $3) from 1 for 16), $2, $3, now())
        ON CONFLICT (user_id, hat_key) DO NOTHING
        "#,
        user_id,
        map_uid,
        slug,
        map_uid
    )
    .execute(pool)
    .await?;
    Ok(true)
}

async fn display_name(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<String, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT display_name FROM users WHERE web_services_user_id = $1
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| row.display_name).unwrap_or_default())
}

/// Empty slug unequips. Non-empty requires a validated unlock. Returns the push entry when state changed.
pub async fn report_equip(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
) -> Result<Option<HatOnMapPlayer>, sqlx::Error> {
    let name = display_name(pool, user_id).await?;
    let wsid = user_id.to_string();
    if slug.is_empty() {
        sqlx::query!(
            r#"--sql
            DELETE FROM hat_equipped WHERE user_id = $1 AND map_uid = $2
            "#,
            user_id,
            map_uid
        )
        .execute(pool)
        .await?;
        return Ok(Some(HatOnMapPlayer {
            wsid,
            name,
            slug: String::new(),
        }));
    }
    let unlocked = sqlx::query!(
        r#"--sql
        SELECT 1 AS ok FROM hat_unlock
        WHERE user_id = $1 AND map_uid = $2 AND slug = $3
          AND hat_key = substring(md5($4 || ':h:' || $3) from 1 for 16)
        "#,
        user_id,
        map_uid,
        slug,
        map_uid
    )
    .fetch_optional(pool)
    .await?;
    if unlocked.is_none() {
        return Ok(None);
    }
    sqlx::query!(
        r#"--sql
        INSERT INTO hat_equipped (user_id, map_uid, slug, hat_key, updated_at)
        VALUES ($1, $2, $3, substring(md5($4 || ':h:' || $3) from 1 for 16), now())
        ON CONFLICT (user_id, map_uid) DO UPDATE SET
            slug = EXCLUDED.slug,
            hat_key = EXCLUDED.hat_key,
            updated_at = now()
        "#,
        user_id,
        map_uid,
        slug,
        map_uid
    )
    .execute(pool)
    .await?;
    Ok(Some(HatOnMapPlayer { wsid, name, slug: slug.to_string() }))
}

pub async fn get_hats_mine(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
) -> Result<(String, Vec<HatMineItem>), sqlx::Error> {
    let equipped = sqlx::query!(
        r#"--sql
        SELECT slug FROM hat_equipped WHERE user_id = $1 AND map_uid = $2
        "#,
        user_id,
        map_uid
    )
    .fetch_optional(pool)
    .await?;
    let equipped = equipped.map(|r| r.slug).unwrap_or_default();
    let rows = sqlx::query!(
        r#"--sql
        SELECT m.slug, m.name, m.hat_key, u.unlocked_at AS "unlocked_at?"
        FROM hat_on_map m
        LEFT JOIN hat_unlock u
          ON u.hat_key = m.hat_key AND u.user_id = $2
        WHERE m.map_uid = $1
        ORDER BY m.slug
        "#,
        map_uid,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let items = rows
        .into_iter()
        .map(|r| HatMineItem {
            slug: r.slug,
            name: r.name,
            key: r.hat_key.trim().to_string(),
            unlocked_at: r.unlocked_at.map(|t| t.timestamp() as u32),
        })
        .collect();
    Ok((equipped, items))
}

/// Equipped hats for players currently on this map. Validated unlocks only. Bans are not applied.
pub async fn get_hats_on_map(pool: &Pool<Postgres>, map_uid: &str) -> Result<Vec<HatOnMapPlayer>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"--sql
        SELECT e.user_id, u.display_name, e.slug
        FROM hat_equipped e
        INNER JOIN hat_unlock ul
          ON ul.user_id = e.user_id AND ul.hat_key = e.hat_key
        INNER JOIN hat_on_map hm
          ON hm.map_uid = e.map_uid AND hm.slug = e.slug
        INNER JOIN map_curr_heights m
          ON m.user_id = e.user_id AND m.map_uid = e.map_uid
        INNER JOIN users u ON u.web_services_user_id = e.user_id
        WHERE e.map_uid = $1
          AND e.slug <> ''
          AND m.updated_at > now() - interval '120 seconds'
        ORDER BY u.display_name, e.slug
        "#,
        map_uid
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| HatOnMapPlayer {
            wsid: r.user_id.to_string(),
            name: r.display_name,
            slug: r.slug,
        })
        .collect())
}

pub async fn equipped_push_entry(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
) -> Result<Option<HatOnMapPlayer>, sqlx::Error> {
    let row = sqlx::query!(
        r#"--sql
        SELECT e.slug, u.display_name
        FROM hat_equipped e
        INNER JOIN hat_unlock ul
          ON ul.user_id = e.user_id AND ul.hat_key = e.hat_key
        INNER JOIN users u ON u.web_services_user_id = e.user_id
        WHERE e.user_id = $1 AND e.map_uid = $2 AND e.slug <> ''
        "#,
        user_id,
        map_uid
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| HatOnMapPlayer {
        wsid: user_id.to_string(),
        name: r.display_name,
        slug: r.slug,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_is_empty() {
        assert!(parse_catalog_hats(&json!({"info":{}})).is_empty());
        assert!(parse_catalog_hats(&json!({"hats":[]})).is_empty());
    }

    #[test]
    fn skips_empty_slug() {
        let spec = json!({"hats":[{"name":"a","slug":"","unlock":{"finishThisMap":true}}]});
        assert!(parse_catalog_hats(&spec).is_empty());
    }

    #[test]
    fn parses_hats_unlock_and_and_uids() {
        let spec = json!({"hats":[
            {"name":"DD1","slug":"Hat-1","unlock":{"recordUids":["DeepDip1___________________"]}},
            {"name":"Both","slug":"Hat-2","unlock":{"finishThisMap":true,"recordUids":["DeepDip1___________________","DeepDip2__The_Storm_Is_Here"]}}
        ]});
        let h = parse_catalog_hats(&spec);
        assert_eq!(h.len(), 2);
        assert!(!h[0].finish_this_map);
        assert_eq!(h[0].record_uids, vec!["DeepDip1___________________"]);
        assert!(h[1].finish_this_map);
        assert_eq!(h[1].record_uids.len(), 2);
    }

    #[test]
    fn last_duplicate_slug_wins() {
        let spec = json!({"hats":{"items":[
            {"name":"old","slug":"Hat-1","unlock":{"finishThisMap":false}},
            {"name":"new","slug":"Hat-1","unlock":{"finishThisMap":true}}
        ]}});
        let h = parse_catalog_hats(&spec);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].name, "new");
        assert!(h[0].finish_this_map);
    }

    #[test]
    fn snake_case_unlock_keys() {
        let spec = json!({"hats":[{"name":"x","slug":"Hat-1","unlock":{"finish_this_map":true,"record_uids":["abc"]}}]});
        let h = parse_catalog_hats(&spec);
        assert!(h[0].finish_this_map);
        assert_eq!(h[0].record_uids, vec!["abc"]);
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Pool<Postgres> {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for hat tests");
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

    fn test_map_uid(prefix: &str) -> String {
        let raw = format!("{}{}", prefix, Uuid::new_v4().simple());
        raw.chars().take(27).collect()
    }

    async fn seed_spec(pool: &Pool<Postgres>, user_id: &Uuid, spec: &Value) -> (i64, String) {
        let name_id = Uuid::new_v4().simple().to_string();
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec(pool, user_id, &name_id, spec)
            .await
            .expect("upsert spec");
        replace_catalog(pool, spec_id, user_id, &name_id, spec)
            .await
            .expect("replace catalog");
        (spec_id, name_id)
    }

    async fn cleanup(pool: &Pool<Postgres>, user_id: &Uuid) {
        let _ = sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(pool)
            .await;
    }

    async fn seed_custom_finish(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str) {
        sqlx::query!(
            r#"--sql
            INSERT INTO custom_map_stats (user_id, map_uid, stats)
            VALUES ($1, $2, $3)
            "#,
            user_id,
            map_uid,
            json!({"extra":{"finish":1},"pb_height":12.0})
        )
        .execute(pool)
        .await
        .expect("seed custom finish");
    }

    async fn seed_map_lb(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, height: f64) {
        sqlx::query!(
            r#"--sql
            INSERT INTO map_leaderboard (map_uid, user_id, height, pos, race_time)
            VALUES ($1, $2, $3, $4, 1000)
            "#,
            map_uid,
            user_id,
            height,
            &[0.0_f64, height, 0.0] as &[f64]
        )
        .execute(pool)
        .await
        .expect("seed map lb");
    }

    async fn seed_live(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str) {
        crate::queries::custom_maps::upsert_map_curr_height(pool, map_uid, user_id, [0.0, 10.0, 0.0], 100)
            .await
            .expect("live pos");
    }

    fn hat_spec(finish: bool, record_uids: Vec<&str>) -> Value {
        json!({"hats":[{
            "name":"Crown",
            "slug":"Hat-1",
            "unlock":{"finishThisMap":finish,"recordUids":record_uids}
        }]})
    }

    #[tokio::test]
    async fn finish_this_map_unlocks_when_extra_finish_present() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-finish-ok").await;
        let map_uid = test_map_uid("HatFin");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;

        assert!(report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("unlock"));
        let n = sqlx::query!("SELECT COUNT(*) AS n FROM hat_unlock WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .n
            .unwrap_or(0);
        assert_eq!(n, 1);

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn finish_this_map_fail_closes_on_height_only() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-finish-height").await;
        let map_uid = test_map_uid("HatHt");
        seed_map_lb(&pool, &user_id, &map_uid, 2000.0).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;

        assert!(!report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("unlock"));
        let n = sqlx::query!("SELECT COUNT(*) AS n FROM hat_unlock WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .n
            .unwrap_or(0);
        assert_eq!(n, 0);

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn record_uid_unknown_fail_closes() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-rec-none").await;
        let map_uid = test_map_uid("HatRec");
        let other = test_map_uid("NoData");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(false, vec![&other])).await;

        assert!(!validate_record_for_uid(&pool, &user_id, &other).await.expect("validate"));
        assert!(!report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("unlock"));

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn record_uids_and_requires_every_uid() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-and").await;
        let map_uid = test_map_uid("HatAnd");
        let a = test_map_uid("RecA");
        let b = test_map_uid("RecB");
        seed_map_lb(&pool, &user_id, &a, 10.0).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(false, vec![&a, &b])).await;

        assert!(!report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("partial and"));
        seed_map_lb(&pool, &user_id, &b, 10.0).await;
        assert!(report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("full and"));

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn no_predicates_fail_closes() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-nopred").await;
        let map_uid = test_map_uid("HatNoP");
        let spec = json!({"hats":[{"name":"Free","slug":"Hat-1"}]});
        let (spec_id, _) = seed_spec(&pool, &user_id, &spec).await;
        assert!(!report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("unlock"));
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn sticky_unlock_survives_lost_proof() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-sticky").await;
        let map_uid = test_map_uid("HatStk");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        assert!(report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("first"));
        sqlx::query!("DELETE FROM custom_map_stats WHERE user_id = $1", user_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("sticky"));
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn equip_requires_unlock_and_unequip_clears() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-equip").await;
        let map_uid = test_map_uid("HatEq");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        bind_map_from_catalog(&pool, &map_uid, spec_id).await.expect("bind");

        assert!(report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("no unlock").is_none());
        assert!(report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("unlock"));
        let on = report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("equip");
        assert_eq!(on.unwrap().slug, "Hat-1");
        let off = report_equip(&pool, &user_id, &map_uid, "").await.expect("unequip");
        assert_eq!(off.unwrap().slug, "");
        let n = sqlx::query!(
            "SELECT COUNT(*) AS n FROM hat_equipped WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await
        .unwrap()
        .n
        .unwrap_or(0);
        assert_eq!(n, 0);

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn relay_skips_unvalidated_equip_row_and_ignores_ban() {
        let pool = test_pool().await;
        let actor = seed_user(&pool, "hat-ban-actor").await;
        let user_id = seed_user(&pool, "hat-ban-target").await;
        let map_uid = test_map_uid("HatBan");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        seed_live(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        assert!(report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("unlock"));
        report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("equip");

        crate::queries::moderation::apply_map_ban(&pool, &map_uid, &user_id, &actor, "private", "public", None)
            .await
            .expect("ban");

        let hats = get_hats_on_map(&pool, &map_uid).await.expect("on map");
        assert!(
            hats.iter().any(|h| h.wsid == user_id.to_string() && h.slug == "Hat-1"),
            "banned player must still relay a validated hat; got {hats:?}"
        );

        sqlx::query!("DELETE FROM hat_unlock WHERE user_id = $1", user_id)
            .execute(&pool)
            .await
            .unwrap();
        let hats = get_hats_on_map(&pool, &map_uid).await.expect("on map after strip");
        assert!(
            !hats.iter().any(|h| h.wsid == user_id.to_string()),
            "must not relay without a validated unlock; got {hats:?}"
        );

        cleanup(&pool, &user_id).await;
        cleanup(&pool, &actor).await;
    }

    #[tokio::test]
    async fn hats_mine_lists_unlocked_and_equipped() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-mine").await;
        let map_uid = test_map_uid("HatMine");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        bind_map_from_catalog(&pool, &map_uid, spec_id).await.expect("bind");
        assert!(report_unlock(&pool, &user_id, &map_uid, "Hat-1", spec_id).await.expect("unlock"));
        report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("equip");
        let (equipped, items) = get_hats_mine(&pool, &map_uid, &user_id).await.expect("mine");
        assert_eq!(equipped, "Hat-1");
        assert_eq!(items.len(), 1);
        assert!(items[0].unlocked_at.is_some());
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn unknown_slug_is_silent() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-unknown").await;
        let map_uid = test_map_uid("HatUnk");
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        assert!(!report_unlock(&pool, &user_id, &map_uid, "no-such", spec_id).await.expect("unlock"));
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn dd2_record_fail_closes_without_leaderboard_row() {
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "hat-dd2-none").await;
        assert!(!validate_record_for_uid(&pool, &user_id, DD2_MAP_UID).await.expect("dd2"));
        assert!(!validate_finish_this_map(&pool, &user_id, DD2_MAP_UID).await.expect("dd2 finish"));
        cleanup(&pool, &user_id).await;
    }
}
