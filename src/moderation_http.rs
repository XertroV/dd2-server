//! HTTP handlers for Map moderation admin API + TM OAuth.

use std::sync::Arc;

use chrono::{Duration, Utc};
use log::warn;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres};
use uuid::Uuid;
use warp::{
    http::{header, Response as HttpResponse, StatusCode},
    Filter, Rejection, Reply,
};

use crate::{
    nadeo::NadeoLiveClient,
    queries::moderation,
    tm_oauth::{self, TmOauthConfig},
};

const SESSION_COOKIE: &str = "dd2_session";
const OAUTH_STATE_COOKIE: &str = "dd2_oauth_state";

#[derive(Clone)]
pub struct ModerationState {
    pub pool: Arc<Pool<Postgres>>,
    pub nadeo: Arc<NadeoLiveClient>,
    pub oauth: Option<TmOauthConfig>,
}

#[derive(Debug, Serialize)]
struct ErrMsg {
    error: String,
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Result<HttpResponse<String>, Rejection> {
    let body = serde_json::to_string(&ErrMsg { error: msg.into() }).unwrap_or_else(|_| "{}".into());
    Ok(HttpResponse::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap())
}

fn json_ok<T: Serialize>(v: &T) -> Result<HttpResponse<String>, Rejection> {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    Ok(HttpResponse::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap())
}

async fn session_user(pool: &Pool<Postgres>, cookie_header: Option<String>) -> Option<(Uuid, String)> {
    let header = cookie_header?;
    let sid = cookie_value(&header, SESSION_COOKIE)?;
    let sid = Uuid::parse_str(sid).ok()?;
    moderation::get_web_session(pool, &sid).await.ok().flatten()
}

fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|part| {
        let (cookie_name, value) = part.trim().split_once('=')?;
        (cookie_name == name).then_some(value)
    })
}

fn secure_cookie_suffix(oauth: &TmOauthConfig) -> &'static str {
    if oauth.redirect_uri.starts_with("https://") {
        "; Secure"
    } else {
        ""
    }
}

pub async fn ensure_map_meta(pool: &Pool<Postgres>, nadeo: &NadeoLiveClient, map_uid: &str) -> Result<moderation::MapMeta, String> {
    if let Some(meta) = moderation::get_map_meta(pool, map_uid).await.map_err(|e| e.to_string())? {
        // refresh if older than 7 days
        if meta.fetched_at > Utc::now() - Duration::days(7) {
            return Ok(meta);
        }
    }
    if !nadeo.is_configured() {
        if let Some(meta) = moderation::get_map_meta(pool, map_uid).await.map_err(|e| e.to_string())? {
            return Ok(meta);
        }
        return Err("map meta missing and Nadeo client not configured".into());
    }
    let (author_id, info) = nadeo.author_id_for_uid(map_uid).await?;
    moderation::upsert_map_meta(pool, map_uid, author_id, info.name.as_deref(), info.thumbnail_url.as_deref())
        .await
        .map_err(|e| e.to_string())
}

pub fn routes(state: ModerationState) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone {
    let st = state.clone();
    let with_state = warp::any().map(move || st.clone());

    let oauth_start = warp::path!("api" / "mod" / "oauth" / "start")
        .and(warp::get())
        .and(with_state.clone())
        .and_then(handle_oauth_start);

    let oauth_callback = warp::path!("api" / "mod" / "oauth" / "callback")
        .and(warp::get())
        .and(with_state.clone())
        .and(warp::query::<OauthCallbackQuery>())
        .and(warp::header::optional::<String>("cookie"))
        .and_then(handle_oauth_callback);

    let logout = warp::path!("api" / "mod" / "logout")
        .and(warp::post())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and_then(handle_logout);

    let me = warp::path!("api" / "mod" / "me")
        .and(warp::get())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and_then(handle_me);

    let get_map = warp::path!("api" / "mod" / "map" / String)
        .and(warp::get())
        .and(warp::path::end())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and_then(handle_get_map);

    let list_bans = warp::path!("api" / "mod" / "map" / String / "bans")
        .and(warp::get())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and_then(handle_list_bans);

    let create_ban = warp::path!("api" / "mod" / "map" / String / "bans")
        .and(warp::post())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and(warp::body::json())
        .and_then(handle_create_ban);

    let lift_ban = warp::path!("api" / "mod" / "map" / String / "bans" / String / "lift")
        .and(warp::post())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and(warp::body::json())
        .and_then(handle_lift_ban);

    let list_delegates = warp::path!("api" / "mod" / "map" / String / "delegates")
        .and(warp::get())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and_then(handle_list_delegates);

    let add_delegate = warp::path!("api" / "mod" / "map" / String / "delegates")
        .and(warp::post())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and(warp::body::json())
        .and_then(handle_add_delegate);

    let remove_delegate = warp::path!("api" / "mod" / "map" / String / "delegates" / String)
        .and(warp::delete())
        .and(with_state.clone())
        .and(warp::header::optional::<String>("cookie"))
        .and_then(handle_remove_delegate);

    let admin_static = warp::path("admin")
        .and(warp::path::end())
        .and(warp::get())
        .map(|| warp::reply::html(ADMIN_HTML));

    oauth_start
        .or(oauth_callback)
        .or(logout)
        .or(me)
        .or(get_map)
        .or(list_bans)
        .or(create_ban)
        .or(lift_ban)
        .or(list_delegates)
        .or(add_delegate)
        .or(remove_delegate)
        .or(admin_static)
}

#[derive(Deserialize)]
struct OauthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn handle_oauth_start(state: ModerationState) -> Result<impl Reply, Rejection> {
    let Some(oauth) = state.oauth.as_ref() else {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "TM OAuth not configured");
    };
    let st = Uuid::new_v4().to_string();
    let expires = Utc::now() + Duration::minutes(10);
    if let Err(e) = moderation::save_oauth_state(state.pool.as_ref(), &st, expires).await {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let url = oauth.authorize_url(&st);
    let state_cookie = format!(
        "{}={}; Path=/api/mod/oauth/callback; HttpOnly; SameSite=Lax; Max-Age=600{}",
        OAUTH_STATE_COOKIE,
        st,
        secure_cookie_suffix(oauth)
    );
    Ok(HttpResponse::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, url)
        .header(header::SET_COOKIE, state_cookie)
        .body(String::new())
        .unwrap())
}

async fn handle_oauth_callback(state: ModerationState, q: OauthCallbackQuery, cookie: Option<String>) -> Result<impl Reply, Rejection> {
    if let Some(err) = q.error {
        return json_err(StatusCode::BAD_REQUEST, format!("oauth error: {}", err));
    }
    let Some(oauth) = state.oauth.as_ref() else {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "TM OAuth not configured");
    };
    let code = q.code.ok_or_else(warp::reject::not_found)?;
    let st = q.state.ok_or_else(warp::reject::not_found)?;
    let state_matches_browser = cookie.as_deref().and_then(|header| cookie_value(header, OAUTH_STATE_COOKIE)) == Some(st.as_str());
    if !state_matches_browser {
        return json_err(StatusCode::BAD_REQUEST, "oauth state does not match browser");
    }
    match moderation::take_oauth_state(state.pool.as_ref(), &st).await {
        Ok(true) => {}
        Ok(false) => return json_err(StatusCode::BAD_REQUEST, "invalid oauth state"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let token = match tm_oauth::exchange_code(oauth, &code).await {
        Ok(t) => t,
        Err(e) => return json_err(StatusCode::BAD_GATEWAY, e),
    };
    let user = match tm_oauth::fetch_user(&token.access_token).await {
        Ok(u) => u,
        Err(e) => return json_err(StatusCode::BAD_GATEWAY, e),
    };
    let user_id = match user.account_uuid() {
        Ok(id) => id,
        Err(e) => return json_err(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    if let Err(e) = moderation::ensure_user(state.pool.as_ref(), &user_id, &user.display_name).await {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let session_id = Uuid::new_v4();
    let expires = Utc::now() + Duration::days(14);
    if let Err(e) = moderation::create_web_session(state.pool.as_ref(), &session_id, &user_id, &user.display_name, expires).await {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_COOKIE,
        session_id,
        14 * 24 * 3600,
        secure_cookie_suffix(oauth)
    );
    let clear_state = format!(
        "{}=; Path=/api/mod/oauth/callback; HttpOnly; SameSite=Lax; Max-Age=0{}",
        OAUTH_STATE_COOKIE,
        secure_cookie_suffix(oauth)
    );
    Ok(HttpResponse::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, "/admin")
        .header(header::SET_COOKIE, cookie)
        .header(header::SET_COOKIE, clear_state)
        .body(String::new())
        .unwrap())
}

async fn handle_logout(state: ModerationState, cookie: Option<String>) -> Result<impl Reply, Rejection> {
    if let Some(header) = cookie {
        if let Some(sid) = header
            .split(';')
            .filter_map(|p| p.trim().strip_prefix(&format!("{}=", SESSION_COOKIE)))
            .next()
        {
            if let Ok(id) = Uuid::parse_str(sid) {
                let _ = moderation::delete_web_session(state.pool.as_ref(), &id).await;
            }
        }
    }
    let clear = format!("{}=; Path=/; HttpOnly; Max-Age=0", SESSION_COOKIE);
    Ok(HttpResponse::builder()
        .status(StatusCode::OK)
        .header(header::SET_COOKIE, clear)
        .header(header::CONTENT_TYPE, "application/json")
        .body("{\"ok\":true}".to_string())
        .unwrap())
}

#[derive(Serialize)]
struct MeResp {
    wsid: String,
    display_name: String,
    is_super_mod: bool,
}

async fn handle_me(state: ModerationState, cookie: Option<String>) -> Result<impl Reply, Rejection> {
    let Some((user_id, name)) = session_user(state.pool.as_ref(), cookie).await else {
        return json_err(StatusCode::UNAUTHORIZED, "not logged in");
    };
    let is_sm = moderation::is_super_mod(state.pool.as_ref(), &user_id).await.unwrap_or(false);
    json_ok(&MeResp {
        wsid: user_id.to_string(),
        display_name: name,
        is_super_mod: is_sm,
    })
}

#[derive(Serialize)]
struct MapResp {
    meta: moderation::MapMeta,
    can_moderate: bool,
    is_author: bool,
    is_delegate: bool,
    is_super_mod: bool,
}

async fn handle_get_map(map_uid: String, state: ModerationState, cookie: Option<String>) -> Result<impl Reply, Rejection> {
    let Some((user_id, _)) = session_user(state.pool.as_ref(), cookie).await else {
        return json_err(StatusCode::UNAUTHORIZED, "not logged in");
    };
    let meta = match ensure_map_meta(state.pool.as_ref(), state.nadeo.as_ref(), &map_uid).await {
        Ok(m) => m,
        Err(e) => return json_err(StatusCode::BAD_GATEWAY, e),
    };
    let is_sm = moderation::is_super_mod(state.pool.as_ref(), &user_id).await.unwrap_or(false);
    let is_author = moderation::is_map_author(state.pool.as_ref(), &map_uid, &user_id)
        .await
        .unwrap_or(false);
    let is_delegate = moderation::is_map_delegate(state.pool.as_ref(), &map_uid, &user_id)
        .await
        .unwrap_or(false);
    let can_moderate = is_sm || is_author || is_delegate;
    json_ok(&MapResp {
        meta,
        can_moderate,
        is_author,
        is_delegate,
        is_super_mod: is_sm,
    })
}

async fn require_moderator(state: &ModerationState, map_uid: &str, cookie: Option<String>) -> Result<Uuid, HttpResponse<String>> {
    let Some((user_id, _)) = session_user(state.pool.as_ref(), cookie).await else {
        return Err(json_err(StatusCode::UNAUTHORIZED, "not logged in").unwrap());
    };
    // ensure meta so author checks work
    if let Err(e) = ensure_map_meta(state.pool.as_ref(), state.nadeo.as_ref(), map_uid).await {
        warn!("ensure_map_meta: {}", e);
    }
    match moderation::can_moderate_map(state.pool.as_ref(), map_uid, &user_id).await {
        Ok(true) => Ok(user_id),
        Ok(false) => Err(json_err(StatusCode::FORBIDDEN, "not a moderator for this map").unwrap()),
        Err(e) => Err(json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).unwrap()),
    }
}

async fn handle_list_bans(map_uid: String, state: ModerationState, cookie: Option<String>) -> Result<impl Reply, Rejection> {
    if let Err(resp) = require_moderator(&state, &map_uid, cookie).await {
        return Ok(resp);
    }
    match moderation::list_active_bans(state.pool.as_ref(), &map_uid).await {
        Ok(bans) => json_ok(&bans),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct CreateBanBody {
    user_id: Option<String>,
    display_name: Option<String>,
    private_reason: String,
    public_message: String,
    expires_at: Option<chrono::DateTime<Utc>>,
}

async fn handle_create_ban(
    map_uid: String,
    state: ModerationState,
    cookie: Option<String>,
    body: CreateBanBody,
) -> Result<impl Reply, Rejection> {
    let actor = match require_moderator(&state, &map_uid, cookie).await {
        Ok(u) => u,
        Err(resp) => return Ok(resp),
    };
    if body.private_reason.trim().is_empty() || body.public_message.trim().is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "private_reason and public_message required");
    }
    if body.expires_at.is_some_and(|expires_at| expires_at <= Utc::now()) {
        return json_err(StatusCode::BAD_REQUEST, "expires_at must be in the future");
    }
    let target = if let Some(id) = body.user_id.as_ref() {
        match Uuid::parse_str(id) {
            Ok(u) => u,
            Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid user_id"),
        }
    } else if let Some(name) = body.display_name.as_ref() {
        match moderation::find_user_id_by_display_name(state.pool.as_ref(), name).await {
            Ok(Some(u)) => u,
            Ok(None) => return json_err(StatusCode::NOT_FOUND, "user not found by display name"),
            Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    } else {
        return json_err(StatusCode::BAD_REQUEST, "user_id or display_name required");
    };

    match moderation::is_protected_from_map_ban(state.pool.as_ref(), &map_uid, &target).await {
        Ok(true) => return json_err(StatusCode::FORBIDDEN, "cannot Map-Ban Map Author, Super-mod, or Map Delegate"),
        Ok(false) => {}
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }

    if let Err(e) = moderation::ensure_user_reference(state.pool.as_ref(), &target, body.display_name.as_deref()).await {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    match moderation::apply_map_ban(
        state.pool.as_ref(),
        &map_uid,
        &target,
        &actor,
        body.private_reason.trim(),
        body.public_message.trim(),
        body.expires_at,
    )
    .await
    {
        Ok(()) => json_ok(&serde_json::json!({"ok": true})),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct LiftBanBody {
    keep_unofficial: bool,
}

async fn handle_lift_ban(
    map_uid: String,
    user_id: String,
    state: ModerationState,
    cookie: Option<String>,
    body: LiftBanBody,
) -> Result<impl Reply, Rejection> {
    let actor = match require_moderator(&state, &map_uid, cookie).await {
        Ok(u) => u,
        Err(resp) => return Ok(resp),
    };
    let target = match Uuid::parse_str(&user_id) {
        Ok(u) => u,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid user_id"),
    };
    match moderation::lift_map_ban(state.pool.as_ref(), &map_uid, &target, &actor, body.keep_unofficial).await {
        Ok(true) => json_ok(&serde_json::json!({"ok": true})),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "no active Map Ban"),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_value_matches_exact_cookie_name() {
        let header = "dd2_oauth_state=state-1; dd2_session=session-1";
        assert_eq!(cookie_value(header, OAUTH_STATE_COOKIE), Some("state-1"));
        assert_eq!(cookie_value(header, SESSION_COOKIE), Some("session-1"));
        assert_eq!(cookie_value("not_dd2_session=value", SESSION_COOKIE), None);
    }

    #[test]
    fn oauth_cookies_are_secure_for_https_redirects_only() {
        let mut oauth = TmOauthConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            redirect_uri: "https://example.test/api/mod/oauth/callback".into(),
        };
        assert_eq!(secure_cookie_suffix(&oauth), "; Secure");
        oauth.redirect_uri = "http://localhost:8077/api/mod/oauth/callback".into();
        assert_eq!(secure_cookie_suffix(&oauth), "");
    }
}

async fn handle_list_delegates(map_uid: String, state: ModerationState, cookie: Option<String>) -> Result<impl Reply, Rejection> {
    if let Err(resp) = require_moderator(&state, &map_uid, cookie).await {
        return Ok(resp);
    }
    match moderation::list_delegates(state.pool.as_ref(), &map_uid).await {
        Ok(d) => json_ok(&d),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct DelegateBody {
    user_id: Option<String>,
    display_name: Option<String>,
}

async fn handle_add_delegate(
    map_uid: String,
    state: ModerationState,
    cookie: Option<String>,
    body: DelegateBody,
) -> Result<impl Reply, Rejection> {
    let actor = match require_moderator(&state, &map_uid, cookie).await {
        Ok(u) => u,
        Err(resp) => return Ok(resp),
    };
    // only author or super-mod can add delegates
    let is_sm = moderation::is_super_mod(state.pool.as_ref(), &actor).await.unwrap_or(false);
    let is_author = moderation::is_map_author(state.pool.as_ref(), &map_uid, &actor)
        .await
        .unwrap_or(false);
    if !is_sm && !is_author {
        return json_err(StatusCode::FORBIDDEN, "only Map Author or Super-mod can add Delegates");
    }
    let target = if let Some(id) = body.user_id.as_ref() {
        match Uuid::parse_str(id) {
            Ok(u) => u,
            Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid user_id"),
        }
    } else if let Some(name) = body.display_name.as_ref() {
        match moderation::find_user_id_by_display_name(state.pool.as_ref(), name).await {
            Ok(Some(u)) => u,
            Ok(None) => return json_err(StatusCode::NOT_FOUND, "user not found"),
            Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    } else {
        return json_err(StatusCode::BAD_REQUEST, "user_id or display_name required");
    };
    match moderation::get_active_ban(state.pool.as_ref(), &map_uid, &target).await {
        Ok(Some(_)) => return json_err(StatusCode::CONFLICT, "lift the active Map Ban before adding this Delegate"),
        Ok(None) => {}
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    if let Err(e) = moderation::ensure_user_reference(state.pool.as_ref(), &target, body.display_name.as_deref()).await {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    match moderation::add_delegate(state.pool.as_ref(), &map_uid, &target, &actor).await {
        Ok(()) => json_ok(&serde_json::json!({"ok": true})),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_remove_delegate(
    map_uid: String,
    user_id: String,
    state: ModerationState,
    cookie: Option<String>,
) -> Result<impl Reply, Rejection> {
    let actor = match require_moderator(&state, &map_uid, cookie).await {
        Ok(u) => u,
        Err(resp) => return Ok(resp),
    };
    let is_sm = moderation::is_super_mod(state.pool.as_ref(), &actor).await.unwrap_or(false);
    let is_author = moderation::is_map_author(state.pool.as_ref(), &map_uid, &actor)
        .await
        .unwrap_or(false);
    if !is_sm && !is_author {
        return json_err(StatusCode::FORBIDDEN, "only Map Author or Super-mod can remove Delegates");
    }
    let target = match Uuid::parse_str(&user_id) {
        Ok(u) => u,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid user_id"),
    };
    match moderation::remove_delegate(state.pool.as_ref(), &map_uid, &target).await {
        Ok(()) => json_ok(&serde_json::json!({"ok": true})),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

const ADMIN_HTML: &str = include_str!("../web/admin.html");
