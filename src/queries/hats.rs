use std::num::NonZeroUsize;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use log::warn;
use lru::LruCache;
use serde_json::{json, Value};
use sqlx::{Pool, Postgres, Transaction};
use uuid::Uuid;

use super::CatalogValidationError;
use crate::nadeo::OfficialRecordVerifier;
use crate::router::{HatMineItem, HatOnMapPlayer};

pub struct CatalogHat {
    pub slug: String,
    pub name: String,
    pub finish_this_map: bool,
    pub record_uids: Vec<String>,
}

pub struct HatEquipRateLimiter {
    state: StdMutex<HatEquipRateState>,
    per_user_interval: Duration,
    global_window: Duration,
    global_limit: u32,
}

struct HatEquipRateState {
    attempts: LruCache<Uuid, Instant>,
    global_started: Instant,
    global_count: u32,
}

impl HatEquipRateLimiter {
    pub fn new(capacity: usize, per_user_interval: Duration, global_window: Duration, global_limit: u32) -> Self {
        Self {
            state: StdMutex::new(HatEquipRateState {
                attempts: LruCache::new(NonZeroUsize::new(capacity.max(1)).expect("hat equip limiter capacity is non-zero")),
                global_started: Instant::now(),
                global_count: 0,
            }),
            per_user_interval,
            global_window,
            global_limit: global_limit.max(1),
        }
    }

    pub fn allow(&self, user_id: &Uuid) -> bool {
        self.allow_at(user_id, Instant::now())
    }

    fn allow_at(&self, user_id: &Uuid, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .attempts
            .peek(user_id)
            .is_some_and(|previous| now.saturating_duration_since(*previous) < self.per_user_interval)
        {
            return false;
        }
        if now.saturating_duration_since(state.global_started) >= self.global_window {
            state.global_started = now;
            state.global_count = 0;
        }
        if state.global_count >= self.global_limit {
            return false;
        }
        state.attempts.put(*user_id, now);
        state.global_count += 1;
        true
    }
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

fn parse_unlock(item: &Value) -> Result<(bool, Vec<String>), CatalogValidationError> {
    let Some(unlock) = item.get("unlock") else {
        return Ok((false, vec![]));
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
    let mut seen = std::collections::HashSet::new();
    if let Some(arr) = raw {
        for v in arr {
            let Some(s) = v.as_str() else { continue };
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            if !seen.insert(s.to_string()) {
                return Err(CatalogValidationError::DuplicateRecordUid { uid: s.to_string() });
            }
            uids.push(s.to_string());
        }
    }
    Ok((finish, uids))
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

/// Replace catalog from `spec.hats[]`. Missing slugs get stable sequential
/// defaults; explicit invalid/blank slugs and canonical duplicates reject.
pub fn parse_catalog_hats(spec: &Value) -> Result<Vec<CatalogHat>, CatalogValidationError> {
    let Some(items) = hats_array(spec) else {
        return Ok(vec![]);
    };
    let mut parsed = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut next_default = 1_u32;
    for item in items {
        if !item.is_object() {
            continue;
        }
        let slug = match item.get("slug") {
            None => format!("Hat-{next_default}"),
            Some(value) => {
                let Some(slug) = value.as_str() else {
                    return Err(CatalogValidationError::BlankSlug { catalog: "hat" });
                };
                let slug = slug.trim();
                if slug.is_empty() {
                    return Err(CatalogValidationError::BlankSlug { catalog: "hat" });
                }
                slug.to_string()
            }
        };
        if let Some(number) = slug.strip_prefix("Hat-").and_then(|suffix| suffix.parse::<u32>().ok()) {
            if number >= next_default {
                next_default = number.saturating_add(1);
            }
        }
        if !seen.insert(slug.clone()) {
            return Err(CatalogValidationError::DuplicateSlug { catalog: "hat", slug });
        }
        let name = item
            .get("name")
            .and_then(|s| s.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&slug)
            .to_string();
        let (finish_this_map, record_uids) = parse_unlock(item)?;
        if !finish_this_map && record_uids.is_empty() {
            warn!("Skipping unconditional hat catalog entry: {}", slug);
            continue;
        }
        parsed.push(CatalogHat {
            slug,
            name,
            finish_this_map,
            record_uids,
        });
    }
    Ok(parsed)
}

pub async fn replace_catalog(pool: &Pool<Postgres>, spec_id: i64, user_id: &Uuid, name_id: &str, spec: &Value) -> Result<(), sqlx::Error> {
    let items = parse_catalog_hats(spec).map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
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
    items: &[CatalogHat],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        DELETE FROM hat_catalog WHERE spec_id = $1
        "#,
        spec_id
    )
    .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await?;
    }
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
    let mut tx = pool.begin().await?;
    let bound = bind_map_from_catalog_in_tx(&mut tx, map_uid, spec_id).await?;
    tx.commit().await?;
    Ok(bound.bound)
}

pub(crate) struct HatBindResult {
    pub bound: bool,
    pub removed_wearers: Vec<HatOnMapPlayer>,
}

/// Remove every equipped hat for a map while preserving enough information to
/// broadcast the removals after the surrounding transaction commits. This is
/// used when promoting a legacy unverified binding: its materialized rows were
/// never authoritative, so even an apparently matching slug/key must not
/// survive owner recovery.
pub(crate) async fn purge_map_equipment_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    map_uid: &str,
) -> Result<Vec<HatOnMapPlayer>, sqlx::Error> {
    let removed = sqlx::query!(
        r#"--sql
        WITH deleted AS (
            DELETE FROM hat_equipped
            WHERE map_uid = $1
            RETURNING user_id
        )
        SELECT d.user_id, u.display_name
        FROM deleted d
        JOIN users u ON u.web_services_user_id = d.user_id
        "#,
        map_uid
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(removed
        .into_iter()
        .map(|row| HatOnMapPlayer {
            wsid: row.user_id.to_string(),
            name: row.display_name,
            slug: String::new(),
        })
        .collect())
}

pub(crate) async fn bind_map_from_catalog_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    map_uid: &str,
    spec_id: i64,
) -> Result<HatBindResult, sqlx::Error> {
    sqlx::query!("DELETE FROM hat_on_map WHERE map_uid = $1", map_uid)
        .execute(&mut **tx)
        .await?;
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
    .execute(&mut **tx)
    .await?;
    let removed = sqlx::query!(
        r#"--sql
        WITH deleted AS (
            DELETE FROM hat_equipped e
            WHERE e.map_uid = $1
              AND NOT EXISTS (
                  SELECT 1 FROM hat_on_map h
                  WHERE h.map_uid = e.map_uid
                    AND h.slug = e.slug
                    AND h.hat_key = e.hat_key
              )
            RETURNING e.user_id
        )
        SELECT d.user_id, u.display_name
        FROM deleted d
        JOIN users u ON u.web_services_user_id = d.user_id
        "#,
        map_uid
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(HatBindResult {
        bound: r.rows_affected() > 0,
        removed_wearers: removed
            .into_iter()
            .map(|row| HatOnMapPlayer {
                wsid: row.user_id.to_string(),
                name: row.display_name,
                slug: String::new(),
            })
            .collect(),
    })
}

#[derive(Clone, PartialEq, Eq)]
struct OnMapRow {
    hat_key: String,
    finish_this_map: bool,
    record_uids: Vec<String>,
}

async fn materialized_row(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64, slug: &str) -> Result<Option<OnMapRow>, sqlx::Error> {
    if crate::queries::listed_maps::resolve_bound_spec(pool, map_uid, spec_id)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let r = sqlx::query!(
        r#"--sql
        SELECT hat_key, finish_this_map, record_uids
        FROM hat_on_map
        WHERE map_uid = $1 AND slug = $2
        "#,
        map_uid,
        slug
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| OnMapRow {
        hat_key: row.hat_key,
        finish_this_map: row.finish_this_map,
        record_uids: uids_from_json(&row.record_uids),
    }))
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
        SELECT hat_key, finish_this_map, record_uids
        FROM hat_on_map
        WHERE map_uid = $1 AND slug = $2
        FOR SHARE
        "#,
        map_uid,
        slug
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(r.map(|row| OnMapRow {
        hat_key: row.hat_key,
        finish_this_map: row.finish_this_map,
        record_uids: uids_from_json(&row.record_uids),
    }))
}

/// Prove a finish through Nadeo's authoritative PersonalBest record. Client
/// stats and locally reported heights are never accepted as finish authority.
pub async fn validate_finish_this_map(verifier: &dyn OfficialRecordVerifier, user_id: &Uuid, map_uid: &str) -> bool {
    match verifier.has_recent_official_record(user_id, map_uid).await {
        Ok(valid) => valid,
        Err(error) => {
            warn!("Current-map finish validation failed closed for map {}: {}", map_uid, error);
            false
        }
    }
}

/// Validate a Nadeo PersonalBest record. Local height rows are deliberately not
/// accepted as proof. API/auth failures fail closed and are logged.
pub async fn validate_record_for_uid(verifier: &dyn OfficialRecordVerifier, user_id: &Uuid, uid: &str) -> bool {
    match verifier.has_official_record(user_id, uid).await {
        Ok(valid) => valid,
        Err(error) => {
            warn!("Official record validation failed closed for map {}: {}", uid, error);
            false
        }
    }
}

async fn validate_hat_unlock(verifier: &dyn OfficialRecordVerifier, user_id: &Uuid, map_uid: &str, row: &OnMapRow) -> bool {
    if !row.finish_this_map && row.record_uids.is_empty() {
        // Nothing the server can prove.
        return false;
    }
    if row.finish_this_map && !validate_finish_this_map(verifier, user_id, map_uid).await {
        return false;
    }
    for uid in &row.record_uids {
        if !validate_record_for_uid(verifier, user_id, uid).await {
            return false;
        }
    }
    true
}

async fn already_unlocked(pool: &Pool<Postgres>, user_id: &Uuid, hat_key: &str) -> Result<bool, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT 1 AS ok FROM hat_unlock
        WHERE user_id = $1 AND hat_key = $2
        "#,
        user_id,
        hat_key
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.is_some())
}

/// Sticky unlock. Unknown slug / failed validation: silent drop. Bans are ignored.
pub async fn report_unlock(
    pool: &Pool<Postgres>,
    verifier: &dyn OfficialRecordVerifier,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
    spec_id: i64,
) -> Result<bool, sqlx::Error> {
    let Some(row) = materialized_row(pool, map_uid, spec_id, slug).await? else {
        return Ok(false);
    };
    if already_unlocked(pool, user_id, &row.hat_key).await? {
        return Ok(true);
    }
    if !verifier.allow_unlock_validation(user_id, map_uid, slug) {
        warn!("Hat unlock validation throttled for user {} hat {}/{}", user_id, map_uid, slug);
        return Ok(false);
    }
    if !validate_hat_unlock(verifier, user_id, map_uid, &row).await {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    let Some(locked_row) = lock_materialized_row(&mut tx, map_uid, spec_id, slug).await? else {
        return Ok(false);
    };
    if locked_row != row {
        return Ok(false);
    }
    sqlx::query!(
        r#"--sql
        INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug, unlocked_at)
        VALUES ($1, $2, $3, $4, now())
        ON CONFLICT (user_id, hat_key) DO NOTHING
        "#,
        user_id,
        locked_row.hat_key,
        map_uid,
        slug
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

async fn display_name(tx: &mut Transaction<'_, Postgres>, user_id: &Uuid) -> Result<String, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT display_name FROM users WHERE web_services_user_id = $1
        "#,
        user_id
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(r.display_name)
}

/// Empty slug unequips. Non-empty requires a validated unlock. Returns the push entry when state changed.
pub async fn report_equip(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, slug: &str) -> Result<Option<HatOnMapPlayer>, sqlx::Error> {
    report_equip_with_before_payload(pool, user_id, map_uid, slug, || Ok(())).await
}

async fn report_equip_with_before_payload<F>(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
    before_payload: F,
) -> Result<Option<HatOnMapPlayer>, sqlx::Error>
where
    F: FnOnce() -> Result<(), sqlx::Error>,
{
    let mut tx = pool.begin().await?;
    let event_slug = if slug.is_empty() {
        let removed = sqlx::query!(
            r#"--sql
            DELETE FROM hat_equipped
            WHERE user_id = $1 AND map_uid = $2
            RETURNING user_id
            "#,
            user_id,
            map_uid
        )
        .fetch_optional(&mut *tx)
        .await?;
        if removed.is_none() {
            return Ok(None);
        }
        String::new()
    } else {
        let binding = sqlx::query!(
            r#"--sql
            SELECT spec_id
            FROM map_listing_bind
            WHERE map_uid = $1
            "#,
            map_uid
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(binding) = binding else {
            return Ok(None);
        };
        if !crate::queries::listed_maps::lock_materialized_spec(&mut tx, map_uid, binding.spec_id).await? {
            return Ok(None);
        }
        let unlocked = sqlx::query!(
            r#"--sql
            SELECT h.hat_key
            FROM hat_unlock u
            INNER JOIN hat_on_map h
              ON h.map_uid = u.map_uid AND h.slug = u.slug AND h.hat_key = u.hat_key
            WHERE u.user_id = $1 AND u.map_uid = $2 AND u.slug = $3
              AND u.hat_key = substring(md5($4 || ':h:' || $3) from 1 for 16)
            FOR SHARE OF h, u
            "#,
            user_id,
            map_uid,
            slug,
            map_uid
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(unlocked) = unlocked else {
            return Ok(None);
        };
        let changed = sqlx::query!(
            r#"--sql
            INSERT INTO hat_equipped (user_id, map_uid, slug, hat_key, updated_at)
            VALUES ($1, $2, $3, $4, now())
            ON CONFLICT (user_id, map_uid) DO UPDATE SET
                slug = EXCLUDED.slug,
                hat_key = EXCLUDED.hat_key,
                updated_at = now()
            WHERE hat_equipped.slug IS DISTINCT FROM EXCLUDED.slug
               OR hat_equipped.hat_key IS DISTINCT FROM EXCLUDED.hat_key
            RETURNING slug
            "#,
            user_id,
            map_uid,
            slug,
            unlocked.hat_key
        )
        .fetch_optional(&mut *tx)
        .await?;
        if changed.is_none() {
            return Ok(None);
        }
        slug.to_string()
    };

    before_payload()?;
    let entry = HatOnMapPlayer {
        wsid: user_id.to_string(),
        name: display_name(&mut tx, user_id).await?,
        slug: event_slug,
    };
    tx.commit().await?;
    Ok(Some(entry))
}

pub async fn get_hats_mine(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid) -> Result<(String, Vec<HatMineItem>), sqlx::Error> {
    let equipped = sqlx::query!(
        r#"--sql
        SELECT e.slug
        FROM hat_equipped e
        INNER JOIN hat_on_map h
          ON h.map_uid = e.map_uid AND h.slug = e.slug AND h.hat_key = e.hat_key
        INNER JOIN map_listing_bind b ON b.map_uid = e.map_uid
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE e.user_id = $1 AND e.map_uid = $2
          AND b.author_verified_at IS NOT NULL
          AND b.verified_author_id = s.user_id
          AND b.materialized_spec_updated_at = s.updated_at
          AND b.materialized_spec_hash = md5(s.spec::text)
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
        INNER JOIN map_listing_bind b ON b.map_uid = m.map_uid
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        LEFT JOIN hat_unlock u
          ON u.hat_key = m.hat_key AND u.user_id = $2
        WHERE m.map_uid = $1
          AND b.author_verified_at IS NOT NULL
          AND b.verified_author_id = s.user_id
          AND b.materialized_spec_updated_at = s.updated_at
          AND b.materialized_spec_hash = md5(s.spec::text)
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
          ON hm.map_uid = e.map_uid AND hm.slug = e.slug AND hm.hat_key = e.hat_key
        INNER JOIN map_curr_heights m
          ON m.user_id = e.user_id AND m.map_uid = e.map_uid
        INNER JOIN users u ON u.web_services_user_id = e.user_id
        INNER JOIN map_listing_bind b ON b.map_uid = e.map_uid
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE e.map_uid = $1
          AND e.slug <> ''
          AND m.updated_at > now() - interval '120 seconds'
          AND b.author_verified_at IS NOT NULL
          AND b.verified_author_id = s.user_id
          AND b.materialized_spec_updated_at = s.updated_at
          AND b.materialized_spec_hash = md5(s.spec::text)
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

pub async fn equipped_push_entry(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str) -> Result<Option<HatOnMapPlayer>, sqlx::Error> {
    let row = sqlx::query!(
        r#"--sql
        SELECT e.slug, u.display_name
        FROM hat_equipped e
        INNER JOIN hat_unlock ul
          ON ul.user_id = e.user_id AND ul.hat_key = e.hat_key
        INNER JOIN hat_on_map hm
          ON hm.map_uid = e.map_uid AND hm.slug = e.slug AND hm.hat_key = e.hat_key
        INNER JOIN users u ON u.web_services_user_id = e.user_id
        INNER JOIN map_listing_bind b ON b.map_uid = e.map_uid
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE e.user_id = $1 AND e.map_uid = $2 AND e.slug <> ''
          AND b.author_verified_at IS NOT NULL
          AND b.verified_author_id = s.user_id
          AND b.materialized_spec_updated_at = s.updated_at
          AND b.materialized_spec_hash = md5(s.spec::text)
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
        assert!(parse_catalog_hats(&json!({"info":{}})).expect("valid catalog").is_empty());
        assert!(parse_catalog_hats(&json!({"hats":[]})).expect("valid catalog").is_empty());
    }

    #[test]
    fn explicit_blank_slug_invalidates_catalog() {
        let spec = json!({"hats":[{"name":"a","slug":"","unlock":{"finishThisMap":true}}]});
        assert!(matches!(
            parse_catalog_hats(&spec),
            Err(CatalogValidationError::BlankSlug { catalog: "hat" })
        ));
    }

    #[test]
    fn parses_hats_unlock_and_and_uids() {
        let spec = json!({"hats":[
            {"name":"DD1","slug":"Hat-1","unlock":{"recordUids":["DeepDip1___________________"]}},
            {"name":"Both","slug":"Hat-2","unlock":{"finishThisMap":true,"recordUids":["DeepDip1___________________","DeepDip2__The_Storm_Is_Here"]}}
        ]});
        let h = parse_catalog_hats(&spec).expect("valid catalog");
        assert_eq!(h.len(), 2);
        assert!(!h[0].finish_this_map);
        assert_eq!(h[0].record_uids, vec!["DeepDip1___________________"]);
        assert!(h[1].finish_this_map);
        assert_eq!(h[1].record_uids.len(), 2);
    }

    #[test]
    fn duplicate_canonical_slug_invalidates_catalog() {
        let spec = json!({"hats":{"items":[
            {"name":"old","slug":"Hat-1","unlock":{"finishThisMap":false}},
            {"name":"new","slug":"Hat-1","unlock":{"finishThisMap":true}}
        ]}});
        assert!(matches!(
            parse_catalog_hats(&spec),
            Err(CatalogValidationError::DuplicateSlug {
                catalog: "hat",
                slug
            }) if slug == "Hat-1"
        ));
    }

    #[test]
    fn snake_case_unlock_keys() {
        let spec = json!({"hats":[{"name":"x","slug":"Hat-1","unlock":{"finish_this_map":true,"record_uids":["abc"]}}]});
        let h = parse_catalog_hats(&spec).expect("valid catalog");
        assert!(h[0].finish_this_map);
        assert_eq!(h[0].record_uids, vec!["abc"]);
    }

    #[test]
    fn unconditional_hat_is_rejected_from_the_catalog() {
        let spec = json!({"hats":[{"name":"Free","slug":"Hat-1"}]});
        assert!(parse_catalog_hats(&spec).expect("valid catalog").is_empty());
    }

    #[test]
    fn missing_slugs_default_after_reserved_and_rejected_hats() {
        let spec = json!({"hats":[
            {"name":"Reserved","slug":"  Hat-4  ","unlock":{"finishThisMap":true}},
            {"name":"Rejected"},
            {"name":"Default","unlock":{"finishThisMap":true}}
        ]});
        let hats = parse_catalog_hats(&spec).expect("valid catalog");
        assert_eq!(hats.len(), 2);
        assert_eq!(hats[0].slug, "Hat-4");
        assert_eq!(hats[1].slug, "Hat-6");
    }

    #[test]
    fn explicit_non_string_slug_invalidates_entire_catalog() {
        let spec = json!({"hats":[
            {"slug":"valid","unlock":{"finishThisMap":true}},
            {"slug":42,"unlock":{"finishThisMap":true}}
        ]});
        assert!(matches!(
            parse_catalog_hats(&spec),
            Err(CatalogValidationError::BlankSlug { catalog: "hat" })
        ));
    }

    #[test]
    fn duplicate_canonical_record_uid_invalidates_catalog() {
        let spec = json!({"hats":[{
            "slug":"Hat-1",
            "unlock":{"recordUids":["  uid-one  ", "uid-one"]}
        }]});
        assert!(matches!(
            parse_catalog_hats(&spec),
            Err(CatalogValidationError::DuplicateRecordUid { uid }) if uid == "uid-one"
        ));
    }

    #[test]
    fn equip_rate_limiter_is_per_user_and_globally_bounded_across_map_uids() {
        let limiter = HatEquipRateLimiter::new(4, Duration::from_millis(250), Duration::from_secs(1), 3);
        let user = Uuid::new_v4();
        let other = Uuid::new_v4();
        let third = Uuid::new_v4();
        let fourth = Uuid::new_v4();
        let start = Instant::now();
        assert!(limiter.allow_at(&user, start));
        assert!(!limiter.allow_at(&user, start + Duration::from_millis(249)));
        assert!(limiter.allow_at(&other, start + Duration::from_millis(1)));
        assert!(limiter.allow_at(&third, start + Duration::from_millis(2)));
        assert!(!limiter.allow_at(&fourth, start + Duration::from_millis(3)));
        assert!(limiter.allow_at(&user, start + Duration::from_secs(1)));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::nadeo::{CachedOfficialRecordVerifier, OfficialRecordBackend};
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    #[derive(Default)]
    struct FakeRecordVerifier {
        records: Mutex<HashSet<(Uuid, String)>>,
        error: Option<String>,
    }

    impl FakeRecordVerifier {
        fn allow(&self, user_id: Uuid, map_uid: &str) {
            self.records.lock().unwrap().insert((user_id, map_uid.to_string()));
        }
    }

    impl OfficialRecordVerifier for FakeRecordVerifier {
        fn has_official_record<'a>(
            &'a self,
            account_id: &'a Uuid,
            map_uid: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
            Box::pin(async move {
                if let Some(error) = &self.error {
                    return Err(error.clone());
                }
                Ok(self.records.lock().unwrap().contains(&(*account_id, map_uid.to_string())))
            })
        }
    }

    #[derive(Default)]
    struct AlwaysRecordBackend {
        calls: AtomicUsize,
    }

    impl OfficialRecordBackend for AlwaysRecordBackend {
        fn fetch_official_record<'a>(
            &'a self,
            _account_id: &'a Uuid,
            _map_uid: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
        }
    }

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

    async fn materialize_spec(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64) {
        bind_map_from_catalog(pool, map_uid, spec_id).await.expect("bind hat snapshot");
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
        .expect("mark hat snapshot materialized");
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
    async fn forged_finish_stats_cannot_unlock_but_authoritative_pb_can() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-finish-ok").await;
        let map_uid = test_map_uid("HatFin");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        materialize_spec(&pool, &map_uid, spec_id).await;

        assert!(!report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("forged stats fail closed"));
        verifier.allow(user_id, &map_uid);
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("authoritative PB unlock"));
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
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-finish-height").await;
        let map_uid = test_map_uid("HatHt");
        seed_map_lb(&pool, &user_id, &map_uid, 2000.0).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        materialize_spec(&pool, &map_uid, spec_id).await;

        assert!(!report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock"));
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
    async fn local_climber_row_is_not_accepted_as_an_official_record() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-rec-none").await;
        let map_uid = test_map_uid("HatRec");
        let other = test_map_uid("NoData");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        seed_map_lb(&pool, &user_id, &other, 1234.0).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(false, vec![&other])).await;
        materialize_spec(&pool, &map_uid, spec_id).await;

        assert!(!validate_record_for_uid(&verifier, &user_id, &other).await);
        assert!(!report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock"));

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn official_record_api_error_fails_closed() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier {
            records: Mutex::new(HashSet::new()),
            error: Some("offline".into()),
        };
        let user_id = seed_user(&pool, "hat-rec-error").await;
        let map_uid = test_map_uid("HatErr");
        let record_uid = test_map_uid("RecErr");
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(false, vec![&record_uid])).await;
        materialize_spec(&pool, &map_uid, spec_id).await;

        assert!(!report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("fail closed"));
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn record_uids_and_requires_every_uid() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-and").await;
        let map_uid = test_map_uid("HatAnd");
        let a = test_map_uid("RecA");
        let b = test_map_uid("RecB");
        verifier.allow(user_id, &a);
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(false, vec![&a, &b])).await;
        materialize_spec(&pool, &map_uid, spec_id).await;

        assert!(!report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("partial and"));
        verifier.allow(user_id, &b);
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("full and"));

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn no_predicates_fail_closes() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-nopred").await;
        let map_uid = test_map_uid("HatNoP");
        let spec = json!({"hats":[{"name":"Free","slug":"Hat-1"}]});
        let (spec_id, _) = seed_spec(&pool, &user_id, &spec).await;
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(!report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock"));
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn sticky_unlock_survives_lost_proof() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-sticky").await;
        let map_uid = test_map_uid("HatStk");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        materialize_spec(&pool, &map_uid, spec_id).await;
        verifier.allow(user_id, &map_uid);
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("first"));
        sqlx::query!("DELETE FROM custom_map_stats WHERE user_id = $1", user_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("sticky"));
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn equip_requires_unlock_and_unequip_clears() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-equip").await;
        let map_uid = test_map_uid("HatEq");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        verifier.allow(user_id, &map_uid);
        materialize_spec(&pool, &map_uid, spec_id).await;

        assert!(report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("no unlock").is_none());
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock"));
        let on = report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("equip");
        assert_eq!(on.unwrap().slug, "Hat-1");
        assert!(report_equip(&pool, &user_id, &map_uid, "Hat-1")
            .await
            .expect("unchanged equip")
            .is_none());
        let off = report_equip(&pool, &user_id, &map_uid, "").await.expect("unequip");
        assert_eq!(off.unwrap().slug, "");
        assert!(report_equip(&pool, &user_id, &map_uid, "")
            .await
            .expect("unchanged unequip")
            .is_none());
        let n = sqlx::query!("SELECT COUNT(*) AS n FROM hat_equipped WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .n
            .unwrap_or(0);
        assert_eq!(n, 0);

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn equip_and_unequip_roll_back_when_payload_construction_fails() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-payload-rollback").await;
        let map_uid = test_map_uid("HatPayload");
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        verifier.allow(user_id, &map_uid);
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock before payload rollback test"));

        let equip_error = report_equip_with_before_payload(&pool, &user_id, &map_uid, "Hat-1", || {
            Err(sqlx::Error::Protocol("injected payload construction failure".into()))
        })
        .await
        .expect_err("candidate equip must roll back");
        assert!(matches!(equip_error, sqlx::Error::Protocol(_)));
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("count rolled-back equip")
            .count
            .unwrap_or(0),
            0
        );

        let equipped = report_equip(&pool, &user_id, &map_uid, "Hat-1")
            .await
            .expect("retry equip")
            .expect("retry still transitions");
        assert_eq!(equipped.name, "hat-payload-rollback");
        assert_eq!(equipped.slug, "Hat-1");

        let unequip_error = report_equip_with_before_payload(&pool, &user_id, &map_uid, "", || {
            Err(sqlx::Error::Protocol("injected payload construction failure".into()))
        })
        .await
        .expect_err("candidate unequip must roll back");
        assert!(matches!(unequip_error, sqlx::Error::Protocol(_)));
        assert_eq!(
            sqlx::query!(
                "SELECT slug FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("rolled-back unequip keeps equipment")
            .slug,
            "Hat-1"
        );

        let unequipped = report_equip(&pool, &user_id, &map_uid, "")
            .await
            .expect("retry unequip")
            .expect("retry still transitions");
        assert_eq!(unequipped.name, "hat-payload-rollback");
        assert!(unequipped.slug.is_empty());
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("retry unequip commits")
            .count
            .unwrap_or(0),
            0
        );

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup payload rollback binding");
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn concurrent_request_81_removal_prevents_stale_equip() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-equip-race").await;
        let map_uid = test_map_uid("HatEqRace");
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        verifier.allow(user_id, &map_uid);
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock before race"));

        let mut rebuild = pool.begin().await.expect("begin request-81-style rebuild");
        sqlx::query!("SELECT id FROM custom_map_aux_specs WHERE id = $1 FOR UPDATE", spec_id)
            .fetch_one(&mut *rebuild)
            .await
            .expect("lock source before binding");
        sqlx::query!("SELECT spec_id FROM map_listing_bind WHERE map_uid = $1 FOR UPDATE", map_uid)
            .fetch_one(&mut *rebuild)
            .await
            .expect("lock binding after source");
        sqlx::query!("DELETE FROM hat_on_map WHERE map_uid = $1 AND slug = 'Hat-1'", map_uid)
            .execute(&mut *rebuild)
            .await
            .expect("remove old request-81 snapshot row");

        let equip_pool = pool.clone();
        let equip_map_uid = map_uid.clone();
        let mut equip = tokio::spawn(async move { report_equip(&equip_pool, &user_id, &equip_map_uid, "Hat-1").await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut equip)
                .await
                .is_err(),
            "equip must wait behind request 81's source/binding locks"
        );

        rebuild.commit().await.expect("commit snapshot removal");
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), equip)
            .await
            .expect("equip unblocks after request 81")
            .expect("equip task joins")
            .expect("equip query succeeds");
        assert!(result.is_none(), "removed snapshot row must fail equip closed");
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("count stale equipment")
            .count
            .unwrap_or(0),
            0
        );

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup race binding");
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn one_fifo_batch_can_unlock_multiple_distinct_hats_without_delay() {
        let pool = test_pool().await;
        let backend = Arc::new(AlwaysRecordBackend::default());
        let verifier = CachedOfficialRecordVerifier::new(backend.clone());
        let user_id = seed_user(&pool, "hat-fifo-distinct").await;
        let map_uid = test_map_uid("HatFifo");
        let record_a = test_map_uid("RecFifoA");
        let record_b = test_map_uid("RecFifoB");
        let spec = json!({"hats":[
            {"slug":"Hat-1","unlock":{"recordUids":[record_a]}},
            {"slug":"Hat-2","unlock":{"recordUids":[record_b]}}
        ]});
        let (spec_id, _) = seed_spec(&pool, &user_id, &spec).await;
        materialize_spec(&pool, &map_uid, spec_id).await;

        assert!(report_unlock(&pool, verifier.as_ref(), &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("first FIFO unlock"));
        assert!(report_unlock(&pool, verifier.as_ref(), &user_id, &map_uid, "Hat-2", spec_id)
            .await
            .expect("second FIFO unlock"));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_unlock WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("count unlocks")
            .count
            .unwrap_or(0),
            2
        );

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn removed_hat_cannot_equip_or_broadcast_and_stale_equipment_is_cleared() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-removed").await;
        let map_uid = test_map_uid("HatGone");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        seed_live(&pool, &user_id, &map_uid).await;
        let (spec_id, name_id) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        verifier.allow(user_id, &map_uid);
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock old hat"));
        assert!(report_equip(&pool, &user_id, &map_uid, "Hat-1")
            .await
            .expect("equip old hat")
            .is_some());

        let replacement = json!({"hats":[
            {"slug":"Hat-2","unlock":{"finishThisMap":true}}
        ]});
        replace_catalog(&pool, spec_id, &user_id, &name_id, &replacement)
            .await
            .expect("replace hat catalog");
        bind_map_from_catalog(&pool, &map_uid, spec_id).await.expect("replace on-map hats");

        assert!(report_equip(&pool, &user_id, &map_uid, "Hat-1")
            .await
            .expect("reject removed hat")
            .is_none());
        assert!(equipped_push_entry(&pool, &user_id, &map_uid)
            .await
            .expect("no removed-hat push")
            .is_none());
        assert!(get_hats_on_map(&pool, &map_uid)
            .await
            .expect("no removed-hat broadcast")
            .iter()
            .all(|hat| hat.wsid != user_id.to_string()));
        let (equipped, items) = get_hats_mine(&pool, &map_uid, &user_id).await.expect("current hats");
        assert!(equipped.is_empty());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].slug, "Hat-2");
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("stale equipment cleared")
            .count
            .unwrap_or(0),
            0
        );

        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn relay_skips_unvalidated_equip_row_and_ignores_ban() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let actor = seed_user(&pool, "hat-ban-actor").await;
        let user_id = seed_user(&pool, "hat-ban-target").await;
        let map_uid = test_map_uid("HatBan");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        seed_live(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        verifier.allow(user_id, &map_uid);
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock"));
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
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-mine").await;
        let map_uid = test_map_uid("HatMine");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        verifier.allow(user_id, &map_uid);
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock"));
        report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("equip");
        let (equipped, items) = get_hats_mine(&pool, &map_uid, &user_id).await.expect("mine");
        assert_eq!(equipped, "Hat-1");
        assert_eq!(items.len(), 1);
        assert!(items[0].unlocked_at.is_some());
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn all_hat_reads_fail_closed_for_unverified_or_stale_binding() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-binding-gate").await;
        let wrong_author = seed_user(&pool, "hat-binding-wrong-author").await;
        let map_uid = test_map_uid("HatGate");
        seed_custom_finish(&pool, &user_id, &map_uid).await;
        seed_live(&pool, &user_id, &map_uid).await;
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        verifier.allow(user_id, &map_uid);
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(report_unlock(&pool, &verifier, &user_id, &map_uid, "Hat-1", spec_id)
            .await
            .expect("unlock"));
        assert!(report_equip(&pool, &user_id, &map_uid, "Hat-1").await.expect("equip").is_some());

        sqlx::query!(
            "UPDATE map_listing_bind SET verified_author_id = $2 WHERE map_uid = $1",
            map_uid,
            wrong_author
        )
        .execute(&pool)
        .await
        .expect("mismatch verified author");
        let (equipped, items) = get_hats_mine(&pool, &map_uid, &user_id).await.expect("wrong-author mine");
        assert!(equipped.is_empty());
        assert!(items.is_empty());
        assert!(get_hats_on_map(&pool, &map_uid).await.expect("wrong-author on-map").is_empty());
        assert!(equipped_push_entry(&pool, &user_id, &map_uid)
            .await
            .expect("wrong-author join push")
            .is_none());
        sqlx::query!(
            "UPDATE map_listing_bind SET verified_author_id = $2 WHERE map_uid = $1",
            map_uid,
            user_id
        )
        .execute(&pool)
        .await
        .expect("restore verified author");

        let assert_hidden = async {
            let (equipped, items) = get_hats_mine(&pool, &map_uid, &user_id).await.expect("mine");
            assert!(equipped.is_empty());
            assert!(items.is_empty());
            assert!(get_hats_on_map(&pool, &map_uid).await.expect("on-map").is_empty());
            assert!(equipped_push_entry(&pool, &user_id, &map_uid).await.expect("join push").is_none());
        };

        sqlx::query!(
            r#"
            UPDATE map_listing_bind
            SET verified_author_id = NULL, author_verified_at = NULL
            WHERE map_uid = $1
            "#,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("make binding unverified");
        assert_hidden.await;

        sqlx::query!(
            r#"
            UPDATE map_listing_bind
            SET verified_author_id = $2, author_verified_at = NOW()
            WHERE map_uid = $1
            "#,
            map_uid,
            user_id
        )
        .execute(&pool)
        .await
        .expect("restore verification");
        sqlx::query!(
            "UPDATE custom_map_aux_specs SET updated_at = updated_at + interval '1 second' WHERE id = $1",
            spec_id
        )
        .execute(&pool)
        .await
        .expect("make snapshot stale");
        let (equipped, items) = get_hats_mine(&pool, &map_uid, &user_id).await.expect("stale mine");
        assert!(equipped.is_empty());
        assert!(items.is_empty());
        assert!(get_hats_on_map(&pool, &map_uid).await.expect("stale on-map").is_empty());
        assert!(equipped_push_entry(&pool, &user_id, &map_uid)
            .await
            .expect("stale join push")
            .is_none());

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("delete test binding");
        cleanup(&pool, &wrong_author).await;
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn unknown_slug_is_silent() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-unknown").await;
        let map_uid = test_map_uid("HatUnk");
        let (spec_id, _) = seed_spec(&pool, &user_id, &hat_spec(true, vec![])).await;
        materialize_spec(&pool, &map_uid, spec_id).await;
        assert!(!report_unlock(&pool, &verifier, &user_id, &map_uid, "no-such", spec_id)
            .await
            .expect("unlock"));
        cleanup(&pool, &user_id).await;
    }

    #[tokio::test]
    async fn dd2_record_fail_closes_without_leaderboard_row() {
        let pool = test_pool().await;
        let verifier = FakeRecordVerifier::default();
        let user_id = seed_user(&pool, "hat-dd2-none").await;
        assert!(!validate_record_for_uid(&verifier, &user_id, crate::consts::DD2_MAP_UID).await);
        assert!(!validate_finish_this_map(&verifier, &user_id, crate::consts::DD2_MAP_UID).await);
        cleanup(&pool, &user_id).await;
    }
}
