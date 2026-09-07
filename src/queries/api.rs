use std::sync::Arc;
use std::time::Duration;

use lazy_static::lazy_static;
use serde_json::Value;
use sqlx::{query, Pool, Postgres};
use uuid::Uuid;
use warp::{
    reject::Reject,
    reply::{Json, Reply, WithStatus},
};

use crate::{
    api_error,
    cache_util::AsyncCache,
    donations,
    queries::get_global_lb_count,
    router::{CollectibleListItem, LeaderboardEntry, MinigameListGame},
};

use super::{get_global_lb, get_users_latest_height, stats};

/// How long rarely-changing GET routes serve from the in-memory cache before
/// re-querying. The data behind them only moves on threshold crossings, spec
/// rebinds, or manual edits, so a minute of staleness is fine and bursts of
/// polls stop hitting Postgres (the listed-maps page query alone scans every
/// leaderboard row on the page for nb_players).
const SLOW_ROUTE_TTL: Duration = Duration::from_secs(60);

lazy_static! {
    static ref CACHED_LISTED_MAPS: AsyncCache<u32, Vec<super::listed_maps::ListedMap>> = Default::default();
    static ref CACHED_LISTED_MAPS_LEN: AsyncCache<(), i64> = Default::default();
    static ref CACHED_MINIGAME_LIST: AsyncCache<String, Vec<MinigameListGame>> = Default::default();
    static ref CACHED_COLLECTIBLE_LIST: AsyncCache<String, Vec<CollectibleListItem>> = Default::default();
    static ref CACHED_DONATIONS: AsyncCache<(), (f64, f64)> = Default::default();
    static ref CACHED_TWITCH_LIST: AsyncCache<(), Value> = Default::default();
}

pub async fn handle_get_global_lb(pool: &Arc<Pool<Postgres>>, page: u32) -> Result<Json, warp::Rejection> {
    let start = (page * 100) as i32;
    let end = (start + 100) as i32;
    let r = get_global_lb(pool, start, end).await;
    match r {
        Ok(r) => Ok(warp::reply::json(
            &r.into_iter().map(|r| Into::<LeaderboardEntry>::into(r)).collect::<Vec<_>>(),
        )),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_global_lb_len(pool: &Arc<Pool<Postgres>>) -> Result<Json, warp::Rejection> {
    let r = get_global_lb_count(pool).await;
    match r {
        Ok(r) => Ok(warp::reply::json(
            &serde_json::json!({ "len": r, "pages": (r as f64 / 100.0).ceil() as i64 }),
        )),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_global_overview(pool: &Pool<Postgres>) -> Result<Json, warp::Rejection> {
    let r = super::get_global_overview(pool).await;
    match r {
        Ok(r) => Ok(warp::reply::json(&r)),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_server_info(pool: &Pool<Postgres>) -> Result<Json, warp::Rejection> {
    let r = super::get_server_info(pool).await;
    match r {
        // send back json object: { nb_players_live: r }
        Ok(r) => Ok(warp::reply::json(&serde_json::json!({ "nb_players_live": r }))),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_user_lb_pos(pool: &Pool<Postgres>, user: String) -> Result<Json, warp::Rejection> {
    let user_id = match Uuid::parse_str(&user) {
        Ok(id) => id,
        Err(e) => {
            return Err(warp::reject::custom(ApiErrRejection::new("Invalid user id")));
        }
    };
    let r = super::get_user_in_lb(pool, &user_id).await;
    let r = match r {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {:?}", e);
            None
        }
    }
    .map(|r| Into::<LeaderboardEntry>::into(r));
    let r: Value = match r {
        Some(r) => serde_json::json!(r),
        None => serde_json::json!(null),
    };
    Ok(warp::reply::json(&r))
}

pub async fn handle_get_user_live_heights(pool: &Pool<Postgres>, user: String) -> Result<Json, warp::Rejection> {
    let user_id = match Uuid::parse_str(&user) {
        Ok(id) => id,
        Err(e) => {
            return Err(warp::reject::custom(ApiErrRejection::new("Invalid user id")));
        }
    };
    let r = get_users_latest_height(pool, &user_id).await;
    match r {
        Ok(r) => Ok(warp::reply::json(&r)),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn get_live_leaderboard(pool: &Pool<Postgres>) -> Result<Json, warp::Rejection> {
    let r = stats::get_live_leaderboard(pool).await;
    match r {
        Ok(r) => Ok(warp::reply::json(&r)),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_donations(pool: &Pool<Postgres>) -> Result<Json, warp::Rejection> {
    let r = CACHED_DONATIONS
        .get_or_insert_with(&(), SLOW_ROUTE_TTL, || async {
            let pp_total = donations::get_prize_pool_total(pool).await?;
            let gfm_total = donations::get_gfm_donations_latest(pool).await?;
            Ok::<(f64, f64), sqlx::Error>((gfm_total, pp_total))
        })
        .await;
    match r {
        Ok((_, (gfm_total, pp_total))) => Ok(warp::reply::json(&serde_json::json!({
            "gfm_total": gfm_total,
            "pp_total": pp_total
        }))),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_twitch_list(pool: &Pool<Postgres>) -> Result<Json, warp::Rejection> {
    let r = CACHED_TWITCH_LIST
        .get_or_insert_with(&(), SLOW_ROUTE_TTL, || get_twitch_profiles_all(pool))
        .await;
    match r {
        Ok((_, r)) => Ok(warp::reply::json(&r)),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(ApiErrRejection::new(&format!("{:?}", e))))
        }
    }
}

pub async fn get_twitch_profiles_all(pool: &Pool<Postgres>) -> Result<serde_json::Value, api_error::Error> {
    let r = query!(
        r#"--sql
            SELECT tu.user_id, tu.twitch_name, u.display_name FROM twitch_usernames tu
            LEFT JOIN users u ON u.web_services_user_id = tu.user_id;
        "#
    )
    .fetch_all(pool)
    .await?;
    let rows = r
        .into_iter()
        .map(|r| {
            serde_json::json!({
                // "user_id": r.user_id.to_string(),
                "twitch_name": r.twitch_name,
                "display_name": r.display_name
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::value::to_value(rows)?)
}

#[derive(Debug)]
pub struct ApiErrRejection {
    pub err: String,
}

impl ApiErrRejection {
    pub fn new(err: &str) -> Self {
        ApiErrRejection { err: err.to_string() }
    }
}

impl Reject for ApiErrRejection {}

impl From<sqlx::Error> for ApiErrRejection {
    fn from(e: sqlx::Error) -> Self {
        ApiErrRejection { err: format!("{:?}", e) }
    }
}

impl From<api_error::Error> for ApiErrRejection {
    fn from(e: api_error::Error) -> Self {
        ApiErrRejection { err: format!("{:?}", e) }
    }
}

pub async fn handle_get_custom_map_aux_spec(pool: &Pool<Postgres>, wsid: String, name_id: String) -> Result<impl Reply, warp::Rejection> {
    let user_id = match Uuid::parse_str(&wsid) {
        Ok(id) => id,
        Err(e) => {
            return Err(warp::reject::custom(ApiErrRejection::new("Invalid user id")));
        }
    };
    let r = super::custom_map_aux_specs::get_spec(pool, &user_id, &name_id).await;
    match r {
        Ok(Some(r)) => Ok(warp::reply::json(&super::custom_map_aux_specs::spec_json_with_id(r.id, &r.spec))),
        // send 403
        Ok(None) => Err(warp::reject::not_found()),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_map_uid_nb_climbers(pool: &Pool<Postgres>, map_uid: String) -> Result<impl Reply, warp::Rejection> {
    let r = super::custom_maps::get_map_nb_playing_live(pool, &map_uid).await;
    match r {
        Ok(r) => Ok(warp::reply::json(&serde_json::json!({ "nb_climbers": r }))),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_map_uid_leaderboard(pool: &Pool<Postgres>, map_uid: String, page: u32) -> Result<impl Reply, warp::Rejection> {
    let r = super::custom_maps::get_map_leaderboard_page(pool, &map_uid, page).await;
    match r {
        Ok(r) => Ok(warp::reply::json(&r)),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_map_uid_leaderboard_len(pool: &Pool<Postgres>, map_uid: String) -> Result<impl Reply, warp::Rejection> {
    let r = super::custom_maps::get_map_leaderboard_len(pool, &map_uid).await;
    match r {
        Ok(r) => Ok(warp::reply::json(
            &serde_json::json!({ "len": r, "pages": (r as f64 / 100.0).ceil() as i64 }),
        )),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_map_uid_live_heights(pool: &Pool<Postgres>, map_uid: String) -> Result<Json, warp::Rejection> {
    let r = super::custom_maps::get_map_live_heights(pool, &map_uid).await;
    match r {
        Ok(r) => Ok(warp::reply::json(&r)),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

fn reject_slug(slug: &str) -> Result<(), warp::Rejection> {
    if slug.contains('/') {
        return Err(warp::reject::not_found());
    }
    Ok(())
}

pub async fn handle_get_map_uid_minigames(pool: &Pool<Postgres>, map_uid: String) -> Result<impl Reply, warp::Rejection> {
    let r = CACHED_MINIGAME_LIST
        .get_or_insert_with(&map_uid, SLOW_ROUTE_TTL, || super::minigames::get_minigame_list(pool, &map_uid))
        .await;
    match r {
        Ok((_, games)) => Ok(warp::reply::json(&serde_json::json!({ "uid": map_uid, "games": games }))),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_map_uid_minigame_leaderboard(
    pool: &Pool<Postgres>,
    map_uid: String,
    slug: String,
    page: u32,
) -> Result<impl Reply, warp::Rejection> {
    reject_slug(&slug)?;
    let r = super::minigames::get_minigame_leaderboard_page(pool, &map_uid, &slug, page).await;
    match r {
        Ok(entries) => Ok(warp::reply::json(&serde_json::json!({
            "uid": map_uid,
            "slug": slug,
            "entries": entries
        }))),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_map_uid_minigame_leaderboard_len(
    pool: &Pool<Postgres>,
    map_uid: String,
    slug: String,
) -> Result<impl Reply, warp::Rejection> {
    reject_slug(&slug)?;
    let r = super::minigames::get_minigame_leaderboard_len(pool, &map_uid, &slug).await;
    match r {
        Ok(r) => Ok(warp::reply::json(
            &serde_json::json!({ "len": r, "pages": (r as f64 / 100.0).ceil() as i64 }),
        )),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_map_uid_collectibles(pool: &Pool<Postgres>, map_uid: String) -> Result<impl Reply, warp::Rejection> {
    let r = CACHED_COLLECTIBLE_LIST
        .get_or_insert_with(&map_uid, SLOW_ROUTE_TTL, || {
            super::collectibles::get_collectible_list(pool, &map_uid)
        })
        .await;
    match r {
        Ok((_, items)) => Ok(warp::reply::json(&serde_json::json!({ "uid": map_uid, "items": items }))),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_listed_maps(pool: &Pool<Postgres>, page: u32) -> Result<impl Reply, warp::Rejection> {
    let r = CACHED_LISTED_MAPS
        .get_or_insert_with(&page, SLOW_ROUTE_TTL, || super::listed_maps::get_listed_maps_page(pool, page))
        .await;
    match r {
        Ok((_, r)) => Ok(warp::reply::json(&r)),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}

pub async fn handle_get_listed_maps_len(pool: &Pool<Postgres>) -> Result<impl Reply, warp::Rejection> {
    let r = CACHED_LISTED_MAPS_LEN
        .get_or_insert_with(&(), SLOW_ROUTE_TTL, || super::listed_maps::get_listed_maps_len(pool))
        .await;
    match r {
        Ok((_, r)) => Ok(warp::reply::json(&serde_json::json!({
            "len": r,
            "pages": super::listed_maps::listed_maps_page_count(r)
        }))),
        Err(e) => {
            eprintln!("Error: {:?}", e);
            Err(warp::reject::custom(Into::<ApiErrRejection>::into(e)))
        }
    }
}
