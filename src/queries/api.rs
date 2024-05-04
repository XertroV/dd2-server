use std::sync::Arc;

use serde_json::Value;
use sqlx::{Pool, Postgres};
use uuid::Uuid;
use warp::{
    reject::Reject,
    reply::{Json, Reply, WithStatus},
};

use crate::router::LeaderboardEntry;

use super::get_global_lb;

pub async fn handle_get_global_lb(pool: &Arc<Pool<Postgres>>) -> Result<Json, warp::Rejection> {
    let r = get_global_lb(pool, 0, 100).await;
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