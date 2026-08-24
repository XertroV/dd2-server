//! Minimal Nadeo Live/Core Services client for map metadata and record proof.
//! Auth: dedicated-server basic auth (preferred) or skip if unset.

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};
use std::{future::Future, pin::Pin, sync::Arc, sync::Mutex as StdMutex};

use base64::Engine;
use log::{info, warn};
use lru::LruCache;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const TOKEN_MAX_AGE: Duration = Duration::from_secs(50 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const RETRY_ATTEMPTS: usize = 3;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(2);
const RECORD_CACHE_CAPACITY: usize = 2048;
const POSITIVE_RECORD_TTL: Duration = Duration::from_secs(15 * 60);
const NEGATIVE_RECORD_TTL: Duration = Duration::from_secs(90);
const UNLOCK_THROTTLE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Deserialize)]
pub struct LiveMapInfo {
    pub uid: String,
    pub author: String,
    pub name: Option<String>,
    #[serde(rename = "thumbnailUrl")]
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CoreMapInfo {
    #[serde(rename = "mapId")]
    map_id: String,
    #[serde(rename = "mapUid")]
    map_uid: String,
    #[serde(rename = "mapType", default)]
    map_type: String,
    #[serde(rename = "hasClones", default)]
    has_clones: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct CoreMapRecord {
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "mapId")]
    map_id: String,
    #[serde(default)]
    removed: bool,
    #[serde(rename = "scopeType", default)]
    scope_type: String,
}

pub trait OfficialRecordVerifier: Send + Sync {
    fn has_official_record<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>>;

    /// Called once per not-yet-unlocked hat report, before any record probes.
    fn allow_unlock_validation(&self, _account_id: &Uuid) -> bool {
        true
    }
}

pub trait OfficialRecordBackend: Send + Sync {
    fn fetch_official_record<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>>;
}

pub trait MapAuthorVerifier: Send + Sync {
    fn author_id_for_uid<'a>(&'a self, map_uid: &'a str) -> Pin<Box<dyn Future<Output = Result<Uuid, String>> + Send + 'a>>;
}

#[derive(Debug, Deserialize)]
struct TokenResp {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
}

struct CachedToken {
    access: String,
    refresh: Option<String>,
    obtained: Instant,
}

struct CachedRecord {
    value: bool,
    obtained: Instant,
}

pub struct CachedOfficialRecordVerifier<B: ?Sized> {
    backend: Arc<B>,
    records: StdMutex<LruCache<(Uuid, String), CachedRecord>>,
    unlock_attempts: StdMutex<LruCache<Uuid, Instant>>,
    positive_ttl: Duration,
    negative_ttl: Duration,
    unlock_throttle: Duration,
}

impl<B: OfficialRecordBackend + ?Sized> CachedOfficialRecordVerifier<B> {
    pub fn new(backend: Arc<B>) -> Arc<Self> {
        Arc::new(Self::with_config(
            backend,
            RECORD_CACHE_CAPACITY,
            POSITIVE_RECORD_TTL,
            NEGATIVE_RECORD_TTL,
            UNLOCK_THROTTLE,
        ))
    }

    fn with_config(backend: Arc<B>, capacity: usize, positive_ttl: Duration, negative_ttl: Duration, unlock_throttle: Duration) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("record cache capacity is non-zero");
        Self {
            backend,
            records: StdMutex::new(LruCache::new(capacity)),
            unlock_attempts: StdMutex::new(LruCache::new(capacity)),
            positive_ttl,
            negative_ttl,
            unlock_throttle,
        }
    }

    fn allow_unlock_validation_at(&self, account_id: &Uuid, now: Instant) -> bool {
        let mut attempts = self.unlock_attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if attempts
            .peek(account_id)
            .is_some_and(|previous| now.saturating_duration_since(*previous) < self.unlock_throttle)
        {
            return false;
        }
        attempts.put(*account_id, now);
        true
    }
}

impl<B: OfficialRecordBackend + ?Sized> OfficialRecordVerifier for CachedOfficialRecordVerifier<B> {
    fn has_official_record<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(async move {
            let key = (*account_id, map_uid.to_string());
            {
                let mut records = self.records.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(cached) = records.get(&key) {
                    let ttl = if cached.value { self.positive_ttl } else { self.negative_ttl };
                    if cached.obtained.elapsed() < ttl {
                        return Ok(cached.value);
                    }
                    records.pop(&key);
                }
            }

            let value = self.backend.fetch_official_record(account_id, map_uid).await?;
            self.records.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).put(
                key,
                CachedRecord {
                    value,
                    obtained: Instant::now(),
                },
            );
            Ok(value)
        })
    }

    fn allow_unlock_validation(&self, account_id: &Uuid) -> bool {
        self.allow_unlock_validation_at(account_id, Instant::now())
    }
}

pub struct NadeoLiveClient {
    http: reqwest::Client,
    user_agent: String,
    login: Option<String>,
    password: Option<String>,
    live_token: RwLock<Option<CachedToken>>,
    core_token: RwLock<Option<CachedToken>>,
    live_refresh: Mutex<()>,
    core_refresh: Mutex<()>,
}

impl NadeoLiveClient {
    pub fn from_env() -> Arc<Self> {
        let login = std::env::var("DD2_NADEO_DS_USERNAME").ok().filter(|s| !s.is_empty());
        let password = std::env::var("DD2_NADEO_DS_PASSWORD").ok().filter(|s| !s.is_empty());
        let email = std::env::var("DD2_NADEO_UA_EMAIL").unwrap_or_else(|_| "dd2-server@xk.io".to_string());
        let user_agent = format!("dd2-server / Max Kaye / {}", email);
        if login.is_none() || password.is_none() {
            warn!("DD2_NADEO_DS_USERNAME/PASSWORD not set; map metadata and official-record validation disabled");
        } else {
            info!("Nadeo Live client configured (dedicated server auth)");
        }
        Arc::new(Self {
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("build bounded Nadeo HTTP client"),
            user_agent,
            login,
            password,
            live_token: RwLock::new(None),
            core_token: RwLock::new(None),
            live_refresh: Mutex::new(()),
            core_refresh: Mutex::new(()),
        })
    }

    pub fn is_configured(&self) -> bool {
        self.login.is_some() && self.password.is_some()
    }

    async fn ensure_token(&self) -> Result<String, String> {
        self.ensure_audience_token(&self.live_token, &self.live_refresh, "NadeoLiveServices")
            .await
    }

    async fn ensure_core_token(&self) -> Result<String, String> {
        self.ensure_audience_token(&self.core_token, &self.core_refresh, "NadeoServices")
            .await
    }

    async fn ensure_audience_token(
        &self,
        cache: &RwLock<Option<CachedToken>>,
        refresh_gate: &Mutex<()>,
        audience: &str,
    ) -> Result<String, String> {
        ensure_cached_token(cache, refresh_gate, || self.authenticate(audience)).await
    }

    async fn authenticate(&self, audience: &str) -> Result<CachedToken, String> {
        let login = self.login.as_ref().ok_or("nadeo not configured")?;
        let password = self.password.as_ref().ok_or("nadeo not configured")?;
        let basic = base64::prelude::BASE64_STANDARD.encode(format!("{}:{}", login, password));
        let resp = send_with_retry(|| {
            self.http
                .post("https://prod.trackmania.core.nadeo.online/v2/authentication/token/basic")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Basic {}", basic))
                .header("User-Agent", &self.user_agent)
                .json(&serde_json::json!({ "audience": audience }))
        })
        .await?;
        if !resp.status().is_success() {
            return Err(format!("nadeo auth status {}", resp.status()));
        }
        let body: TokenResp = resp.json().await.map_err(|e| e.to_string())?;
        Ok(CachedToken {
            access: body.access_token,
            refresh: body.refresh_token,
            obtained: Instant::now(),
        })
    }

    pub async fn get_map_info(&self, map_uid: &str) -> Result<LiveMapInfo, String> {
        if !self.is_configured() {
            return Err("nadeo not configured".into());
        }
        let token = self.ensure_token().await?;
        let url = format!("https://live-services.trackmania.nadeo.live/api/token/map/{}", map_uid);
        let resp = send_with_retry(|| {
            self.http
                .get(&url)
                .header("Authorization", format!("nadeo_v1 t={}", token))
                .header("User-Agent", &self.user_agent)
        })
        .await?;
        if !resp.status().is_success() {
            return Err(format!("map info status {}", resp.status()));
        }
        resp.json::<LiveMapInfo>().await.map_err(|e| e.to_string())
    }

    pub async fn author_id_for_uid(&self, map_uid: &str) -> Result<(Uuid, LiveMapInfo), String> {
        let info = self.get_map_info(map_uid).await?;
        let author_id = Uuid::parse_str(&info.author).map_err(|e| e.to_string())?;
        Ok((author_id, info))
    }

    /// Prove an arbitrary account has a Nadeo PersonalBest record for this map.
    /// This uses the authoritative Core API rather than locally reported climb
    /// heights. Any auth, lookup, schema, or transport failure is an error so
    /// callers can fail closed.
    async fn has_official_record_impl(&self, account_id: &Uuid, map_uid: &str) -> Result<bool, String> {
        if map_uid.len() != 27 {
            return Ok(false);
        }
        if !self.is_configured() {
            return Err("nadeo not configured".into());
        }
        let token = self.ensure_core_token().await?;
        let map_resp = send_with_retry(|| {
            self.http
                .get("https://prod.trackmania.core.nadeo.online/maps/by-uid/")
                .header("Authorization", format!("nadeo_v1 t={}", token))
                .header("User-Agent", &self.user_agent)
                .query(&[("mapUidList", map_uid)])
        })
        .await?;
        if !map_resp.status().is_success() {
            return Err(format!("core map info status {}", map_resp.status()));
        }
        let maps: Vec<CoreMapInfo> = map_resp.json().await.map_err(|e| e.to_string())?;
        let Some(map) = maps.into_iter().find(|map| map.map_uid == map_uid) else {
            return Ok(false);
        };

        let account_id = account_id.to_string();
        let game_mode = core_game_mode(&map);
        let record_resp = send_with_retry(|| {
            self.http
                .get("https://prod.trackmania.core.nadeo.online/v2/mapRecords/by-account/")
                .header("Authorization", format!("nadeo_v1 t={}", token))
                .header("User-Agent", &self.user_agent)
                .query(&[
                    ("accountIdList", account_id.as_str()),
                    ("mapId", map.map_id.as_str()),
                    ("gameMode", game_mode),
                ])
        })
        .await?;
        if !record_resp.status().is_success() {
            return Err(format!("core map record status {}", record_resp.status()));
        }
        let records: Vec<CoreMapRecord> = record_resp.json().await.map_err(|e| e.to_string())?;
        Ok(records_contain_personal_best(&records, &account_id, &map.map_id))
    }
}

async fn ensure_cached_token<F, Fut>(cache: &RwLock<Option<CachedToken>>, refresh_gate: &Mutex<()>, fetch: F) -> Result<String, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<CachedToken, String>>,
{
    if let Some(access) = fresh_token(cache).await {
        return Ok(access);
    }
    let _refresh = refresh_gate.lock().await;
    if let Some(access) = fresh_token(cache).await {
        return Ok(access);
    }
    let token = fetch().await?;
    let access = token.access.clone();
    *cache.write().await = Some(token);
    Ok(access)
}

async fn fresh_token(cache: &RwLock<Option<CachedToken>>) -> Option<String> {
    cache
        .read()
        .await
        .as_ref()
        .filter(|token| token.obtained.elapsed() < TOKEN_MAX_AGE)
        .map(|token| token.access.clone())
}

fn retry_delay(status: StatusCode, retry_after: Option<&str>, attempt: usize) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
        return None;
    }
    if let Some(seconds) = retry_after.and_then(|value| value.trim().parse::<u64>().ok()) {
        return Some(Duration::from_secs(seconds).min(MAX_RETRY_DELAY));
    }
    Some(Duration::from_millis(200 * (1_u64 << attempt.min(3))).min(MAX_RETRY_DELAY))
}

async fn send_with_retry<F>(mut request: F) -> Result<Response, String>
where
    F: FnMut() -> RequestBuilder,
{
    for attempt in 0..RETRY_ATTEMPTS {
        match request().send().await {
            Ok(response) => {
                let delay = retry_delay(
                    response.status(),
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok()),
                    attempt,
                );
                if let Some(delay) = delay.filter(|_| attempt + 1 < RETRY_ATTEMPTS) {
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Ok(response);
            }
            Err(error) if attempt + 1 < RETRY_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(200 * (1_u64 << attempt.min(3)))).await;
                warn!("Transient Nadeo transport failure (attempt {}): {}", attempt + 1, error);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!("bounded retry loop always returns")
}

fn core_game_mode(map: &CoreMapInfo) -> &'static str {
    if map.map_type.ends_with("TM_Stunt") {
        "Stunt"
    } else if map.map_type.ends_with("TM_Platform") {
        "Platform"
    } else if map.has_clones {
        "TimeAttackClone"
    } else {
        "TimeAttack"
    }
}

fn records_contain_personal_best(records: &[CoreMapRecord], account_id: &str, map_id: &str) -> bool {
    records
        .iter()
        .any(|record| record.account_id == account_id && record.map_id == map_id && !record.removed && record.scope_type == "PersonalBest")
}

impl OfficialRecordBackend for NadeoLiveClient {
    fn fetch_official_record<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(self.has_official_record_impl(account_id, map_uid))
    }
}

impl MapAuthorVerifier for NadeoLiveClient {
    fn author_id_for_uid<'a>(&'a self, map_uid: &'a str) -> Pin<Box<dyn Future<Output = Result<Uuid, String>> + Send + 'a>> {
        Box::pin(async move { self.author_id_for_uid(map_uid).await.map(|(author_id, _)| author_id) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeRecordBackend {
        calls: AtomicUsize,
        results: StdMutex<VecDeque<Result<bool, String>>>,
    }

    impl FakeRecordBackend {
        fn new(results: impl IntoIterator<Item = Result<bool, String>>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                results: StdMutex::new(results.into_iter().collect()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl OfficialRecordBackend for FakeRecordBackend {
        fn fetch_official_record<'a>(
            &'a self,
            _account_id: &'a Uuid,
            _map_uid: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.results
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop_front()
                    .expect("fake backend result")
            })
        }
    }

    fn cached(backend: Arc<FakeRecordBackend>, capacity: usize) -> CachedOfficialRecordVerifier<FakeRecordBackend> {
        CachedOfficialRecordVerifier::with_config(
            backend,
            capacity,
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(2),
        )
    }

    fn map(map_type: &str, has_clones: bool) -> CoreMapInfo {
        CoreMapInfo {
            map_id: "map-id".into(),
            map_uid: "abcdefghijklmnopqrstuvwxyza".into(),
            map_type: map_type.into(),
            has_clones,
        }
    }

    #[test]
    fn core_game_mode_covers_special_and_clone_maps() {
        assert_eq!(core_game_mode(&map("TrackMania\\TM_Race", false)), "TimeAttack");
        assert_eq!(core_game_mode(&map("TrackMania\\TM_Race", true)), "TimeAttackClone");
        assert_eq!(core_game_mode(&map("TrackMania\\TM_Stunt", false)), "Stunt");
        assert_eq!(core_game_mode(&map("Trackmania\\TM_Platform", false)), "Platform");
    }

    #[test]
    fn only_exact_active_personal_best_is_accepted() {
        let good = CoreMapRecord {
            account_id: "account".into(),
            map_id: "map".into(),
            removed: false,
            scope_type: "PersonalBest".into(),
        };
        assert!(records_contain_personal_best(&[good.clone()], "account", "map"));
        for bad in [
            CoreMapRecord {
                account_id: "other".into(),
                ..good.clone()
            },
            CoreMapRecord {
                map_id: "other".into(),
                ..good.clone()
            },
            CoreMapRecord {
                removed: true,
                ..good.clone()
            },
            CoreMapRecord {
                scope_type: "Seasonal".into(),
                ..good.clone()
            },
        ] {
            assert!(!records_contain_personal_best(&[bad], "account", "map"));
        }
    }

    #[tokio::test]
    async fn positive_and_negative_record_results_are_cached() {
        let user = Uuid::new_v4();
        for value in [true, false] {
            let backend = FakeRecordBackend::new([Ok(value)]);
            let verifier = cached(backend.clone(), 4);
            assert_eq!(verifier.has_official_record(&user, "Map________________________").await, Ok(value));
            assert_eq!(verifier.has_official_record(&user, "Map________________________").await, Ok(value));
            assert_eq!(backend.calls(), 1);
        }
    }

    #[tokio::test]
    async fn record_errors_are_not_cached() {
        let user = Uuid::new_v4();
        let backend = FakeRecordBackend::new([Err("temporary".into()), Ok(true)]);
        let verifier = cached(backend.clone(), 4);
        assert_eq!(
            verifier.has_official_record(&user, "Map________________________").await,
            Err("temporary".into())
        );
        assert_eq!(verifier.has_official_record(&user, "Map________________________").await, Ok(true));
        assert_eq!(backend.calls(), 2);
    }

    #[tokio::test]
    async fn record_cache_has_bounded_lru_eviction() {
        let user = Uuid::new_v4();
        let backend = FakeRecordBackend::new([Ok(true), Ok(true), Ok(true), Ok(false)]);
        let verifier = cached(backend.clone(), 2);
        assert!(verifier.has_official_record(&user, "A__________________________").await.unwrap());
        assert!(verifier.has_official_record(&user, "B__________________________").await.unwrap());
        assert!(verifier.has_official_record(&user, "C__________________________").await.unwrap());
        assert!(!verifier.has_official_record(&user, "A__________________________").await.unwrap());
        assert_eq!(backend.calls(), 4);
    }

    #[test]
    fn unlock_validation_throttle_is_per_user_and_bounded_in_time() {
        let backend = FakeRecordBackend::new([]);
        let verifier = cached(backend, 4);
        let user = Uuid::new_v4();
        let other = Uuid::new_v4();
        let start = Instant::now();
        assert!(verifier.allow_unlock_validation_at(&user, start));
        assert!(!verifier.allow_unlock_validation_at(&user, start + Duration::from_secs(1)));
        assert!(verifier.allow_unlock_validation_at(&other, start + Duration::from_secs(1)));
        assert!(verifier.allow_unlock_validation_at(&user, start + Duration::from_secs(2)));
    }

    #[test]
    fn retry_policy_is_transient_bounded_and_honors_numeric_retry_after() {
        assert_eq!(retry_delay(StatusCode::NOT_FOUND, None, 0), None);
        assert_eq!(
            retry_delay(StatusCode::INTERNAL_SERVER_ERROR, None, 0),
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            retry_delay(StatusCode::TOO_MANY_REQUESTS, Some("1"), 0),
            Some(Duration::from_secs(1))
        );
        assert_eq!(retry_delay(StatusCode::TOO_MANY_REQUESTS, Some("99"), 0), Some(MAX_RETRY_DELAY));
    }

    #[tokio::test]
    async fn token_refresh_is_single_flight() {
        let cache = Arc::new(RwLock::new(None));
        let gate = Arc::new(Mutex::new(()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let gate = gate.clone();
            let fetches = fetches.clone();
            tasks.push(tokio::spawn(async move {
                ensure_cached_token(cache.as_ref(), gate.as_ref(), || async move {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(CachedToken {
                        access: "token".into(),
                        refresh: None,
                        obtained: Instant::now(),
                    })
                })
                .await
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), "token");
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }
}
