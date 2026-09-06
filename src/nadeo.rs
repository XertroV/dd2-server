//! Minimal Nadeo Live/Core Services client for map metadata and record proof.
//! Auth: dedicated-server basic auth (preferred) or skip if unset.

use std::num::NonZeroUsize;
use std::time::{Duration, Instant, SystemTime};
use std::{future::Future, pin::Pin, sync::Arc, sync::Mutex as StdMutex};

use base64::Engine;
use log::{info, warn};
use lru::LruCache;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock, Semaphore};
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
const RECORD_ADMISSION_WINDOW: Duration = Duration::from_secs(60);
const RECORD_GLOBAL_LIMIT: u32 = 128;
const RECORD_ACCOUNT_LIMIT: u32 = 16;
const RECORD_CONCURRENCY: usize = 16;
const RECENT_RECORD_ATTEMPTS: usize = 3;
const MAP_AUTHOR_CACHE_CAPACITY: usize = 1024;
const MAP_AUTHOR_POSITIVE_TTL: Duration = Duration::from_secs(15 * 60);
const MAP_AUTHOR_NEGATIVE_TTL: Duration = Duration::from_secs(60);
const MAP_AUTHOR_ADMISSION_WINDOW: Duration = Duration::from_secs(60);
const MAP_AUTHOR_GLOBAL_LIMIT: u32 = 64;
const MAP_AUTHOR_ACCOUNT_LIMIT: u32 = 8;
const MAP_AUTHOR_CONCURRENCY: usize = 8;

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

    /// Recheck a just-finished map against Nadeo, bypassing only stale negative
    /// cache entries. Implementations may retry briefly for Nadeo's eventual
    /// consistency, but must remain bounded.
    fn has_recent_official_record<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        self.has_official_record(account_id, map_uid)
    }

    /// Called once per not-yet-unlocked hat report, before any record probes.
    fn allow_unlock_validation(&self, _account_id: &Uuid, _map_uid: &str, _hat_slug: &str) -> bool {
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
    fn author_id_for_uid<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Uuid, String>> + Send + 'a>>;
}

pub trait MapAuthorBackend: Send + Sync {
    fn fetch_map_author<'a>(&'a self, map_uid: &'a str) -> Pin<Box<dyn Future<Output = Result<Option<Uuid>, String>> + Send + 'a>>;
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

struct CachedMapAuthor {
    value: Option<Uuid>,
    obtained: Instant,
}

struct AdmissionWindow {
    started: Instant,
    count: u32,
}

pub struct CachedMapAuthorVerifier<B: ?Sized> {
    backend: Arc<B>,
    authors: StdMutex<LruCache<String, CachedMapAuthor>>,
    key_gates: StdMutex<LruCache<String, Arc<Mutex<()>>>>,
    account_windows: StdMutex<LruCache<Uuid, AdmissionWindow>>,
    global_window: StdMutex<AdmissionWindow>,
    outbound: Semaphore,
    positive_ttl: Duration,
    negative_ttl: Duration,
    admission_window: Duration,
    global_limit: u32,
    account_limit: u32,
}

impl<B: MapAuthorBackend + ?Sized> CachedMapAuthorVerifier<B> {
    pub fn new(backend: Arc<B>) -> Arc<Self> {
        Arc::new(Self::with_config(
            backend,
            MAP_AUTHOR_CACHE_CAPACITY,
            MAP_AUTHOR_POSITIVE_TTL,
            MAP_AUTHOR_NEGATIVE_TTL,
            MAP_AUTHOR_ADMISSION_WINDOW,
            MAP_AUTHOR_GLOBAL_LIMIT,
            MAP_AUTHOR_ACCOUNT_LIMIT,
            MAP_AUTHOR_CONCURRENCY,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn with_config(
        backend: Arc<B>,
        capacity: usize,
        positive_ttl: Duration,
        negative_ttl: Duration,
        admission_window: Duration,
        global_limit: u32,
        account_limit: u32,
        concurrency: usize,
    ) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("map author cache capacity is non-zero");
        Self {
            backend,
            authors: StdMutex::new(LruCache::new(capacity)),
            key_gates: StdMutex::new(LruCache::new(capacity)),
            account_windows: StdMutex::new(LruCache::new(capacity)),
            global_window: StdMutex::new(AdmissionWindow {
                started: Instant::now(),
                count: 0,
            }),
            outbound: Semaphore::new(concurrency.max(1)),
            positive_ttl,
            negative_ttl,
            admission_window,
            global_limit,
            account_limit,
        }
    }

    fn cached_author(&self, map_uid: &str) -> Option<Result<Uuid, String>> {
        let mut authors = self.authors.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (value, expired) = {
            let cached = authors.get(map_uid)?;
            let ttl = if cached.value.is_some() {
                self.positive_ttl
            } else {
                self.negative_ttl
            };
            (cached.value, cached.obtained.elapsed() >= ttl)
        };
        if expired {
            authors.pop(map_uid);
            return None;
        }
        Some(value.ok_or_else(|| "Nadeo map author not found".to_string()))
    }

    fn key_gate(&self, map_uid: &str) -> Result<Arc<Mutex<()>>, String> {
        let mut gates = self.key_gates.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(gate) = gates.get(map_uid) {
            return Ok(gate.clone());
        }
        if gates.len() >= gates.cap().get() {
            let idle_key = gates
                .iter()
                .find(|(_, gate)| Arc::strong_count(gate) == 1)
                .map(|(key, _)| key.clone());
            let Some(idle_key) = idle_key else {
                return Err("Nadeo map author single-flight capacity exceeded".to_string());
            };
            gates.pop(&idle_key);
        }
        let gate = Arc::new(Mutex::new(()));
        gates.put(map_uid.to_string(), gate.clone());
        Ok(gate)
    }

    fn admit_at(&self, account_id: &Uuid, now: Instant) -> bool {
        let mut global = self.global_window.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.saturating_duration_since(global.started) >= self.admission_window {
            *global = AdmissionWindow { started: now, count: 0 };
        }
        if global.count >= self.global_limit {
            return false;
        }

        let mut accounts = self.account_windows.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(window) = accounts.get_mut(account_id) {
            if now.saturating_duration_since(window.started) >= self.admission_window {
                *window = AdmissionWindow { started: now, count: 0 };
            }
            if window.count >= self.account_limit {
                return false;
            }
            window.count += 1;
        } else {
            accounts.put(*account_id, AdmissionWindow { started: now, count: 1 });
        }
        global.count += 1;
        true
    }
}

impl<B: MapAuthorBackend + ?Sized> MapAuthorVerifier for CachedMapAuthorVerifier<B> {
    fn author_id_for_uid<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Uuid, String>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(cached) = self.cached_author(map_uid) {
                return cached;
            }
            let gate = self.key_gate(map_uid)?;
            let _key_guard = gate.lock().await;
            if let Some(cached) = self.cached_author(map_uid) {
                return cached;
            }
            if !self.admit_at(account_id, Instant::now()) {
                return Err("Nadeo map author lookup admission limit exceeded".to_string());
            }
            let _outbound = self
                .outbound
                .acquire()
                .await
                .map_err(|_| "Nadeo map author lookup admission closed".to_string())?;
            let value = self.backend.fetch_map_author(map_uid).await?;
            self.authors.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).put(
                map_uid.to_string(),
                CachedMapAuthor {
                    value,
                    obtained: Instant::now(),
                },
            );
            value.ok_or_else(|| "Nadeo map author not found".to_string())
        })
    }
}

pub struct CachedOfficialRecordVerifier<B: ?Sized> {
    backend: Arc<B>,
    records: StdMutex<LruCache<(Uuid, String), CachedRecord>>,
    key_gates: StdMutex<LruCache<(Uuid, String), Arc<Mutex<()>>>>,
    account_windows: StdMutex<LruCache<Uuid, AdmissionWindow>>,
    global_window: StdMutex<AdmissionWindow>,
    outbound: Semaphore,
    unlock_attempts: StdMutex<LruCache<(Uuid, String, String), Instant>>,
    positive_ttl: Duration,
    negative_ttl: Duration,
    unlock_throttle: Duration,
    admission_window: Duration,
    global_limit: u32,
    account_limit: u32,
}

impl<B: OfficialRecordBackend + ?Sized> CachedOfficialRecordVerifier<B> {
    pub fn new(backend: Arc<B>) -> Arc<Self> {
        Arc::new(Self::with_config(
            backend,
            RECORD_CACHE_CAPACITY,
            POSITIVE_RECORD_TTL,
            NEGATIVE_RECORD_TTL,
            UNLOCK_THROTTLE,
            RECORD_ADMISSION_WINDOW,
            RECORD_GLOBAL_LIMIT,
            RECORD_ACCOUNT_LIMIT,
            RECORD_CONCURRENCY,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn with_config(
        backend: Arc<B>,
        capacity: usize,
        positive_ttl: Duration,
        negative_ttl: Duration,
        unlock_throttle: Duration,
        admission_window: Duration,
        global_limit: u32,
        account_limit: u32,
        concurrency: usize,
    ) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("record cache capacity is non-zero");
        Self {
            backend,
            records: StdMutex::new(LruCache::new(capacity)),
            key_gates: StdMutex::new(LruCache::new(capacity)),
            account_windows: StdMutex::new(LruCache::new(capacity)),
            global_window: StdMutex::new(AdmissionWindow {
                started: Instant::now(),
                count: 0,
            }),
            outbound: Semaphore::new(concurrency.max(1)),
            unlock_attempts: StdMutex::new(LruCache::new(capacity)),
            positive_ttl,
            negative_ttl,
            unlock_throttle,
            admission_window,
            global_limit,
            account_limit,
        }
    }

    fn cached_record(&self, key: &(Uuid, String), negative_not_before: Option<Instant>) -> Option<bool> {
        let mut records = self.records.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (value, obtained, expired) = {
            let cached = records.get(key)?;
            let ttl = if cached.value { self.positive_ttl } else { self.negative_ttl };
            (cached.value, cached.obtained, cached.obtained.elapsed() >= ttl)
        };
        if expired {
            records.pop(key);
            return None;
        }
        if !value && negative_not_before.is_some_and(|required| obtained < required) {
            return None;
        }
        Some(value)
    }

    fn key_gate(&self, key: &(Uuid, String)) -> Result<Arc<Mutex<()>>, String> {
        let mut gates = self.key_gates.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(gate) = gates.get(key) {
            return Ok(gate.clone());
        }
        if gates.len() >= gates.cap().get() {
            let idle_key = gates
                .iter()
                .find(|(_, gate)| Arc::strong_count(gate) == 1)
                .map(|(key, _)| key.clone());
            let Some(idle_key) = idle_key else {
                return Err("Nadeo official-record single-flight capacity exceeded".to_string());
            };
            gates.pop(&idle_key);
        }
        let gate = Arc::new(Mutex::new(()));
        gates.put(key.clone(), gate.clone());
        Ok(gate)
    }

    fn admit_at(&self, account_id: &Uuid, now: Instant) -> bool {
        let mut global = self.global_window.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.saturating_duration_since(global.started) >= self.admission_window {
            *global = AdmissionWindow { started: now, count: 0 };
        }
        if global.count >= self.global_limit {
            return false;
        }
        let mut accounts = self.account_windows.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(window) = accounts.get_mut(account_id) {
            if now.saturating_duration_since(window.started) >= self.admission_window {
                *window = AdmissionWindow { started: now, count: 0 };
            }
            if window.count >= self.account_limit {
                return false;
            }
            window.count += 1;
        } else {
            accounts.put(*account_id, AdmissionWindow { started: now, count: 1 });
        }
        global.count += 1;
        true
    }

    async fn lookup(&self, account_id: &Uuid, map_uid: &str, negative_not_before: Option<Instant>) -> Result<bool, String> {
        let key = (*account_id, map_uid.to_string());
        if let Some(value) = self.cached_record(&key, negative_not_before) {
            return Ok(value);
        }
        let gate = self.key_gate(&key)?;
        let _key_guard = gate.lock().await;
        if let Some(value) = self.cached_record(&key, negative_not_before) {
            return Ok(value);
        }
        if !self.admit_at(account_id, Instant::now()) {
            return Err("Nadeo official-record lookup admission limit exceeded".to_string());
        }
        let _outbound = self
            .outbound
            .acquire()
            .await
            .map_err(|_| "Nadeo official-record lookup admission closed".to_string())?;
        let value = self.backend.fetch_official_record(account_id, map_uid).await?;
        self.records.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).put(
            key,
            CachedRecord {
                value,
                obtained: Instant::now(),
            },
        );
        Ok(value)
    }

    fn allow_unlock_validation_at(&self, account_id: &Uuid, map_uid: &str, hat_slug: &str, now: Instant) -> bool {
        let key = (*account_id, map_uid.to_string(), hat_slug.to_string());
        let mut attempts = self.unlock_attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if attempts
            .peek(&key)
            .is_some_and(|previous| now.saturating_duration_since(*previous) < self.unlock_throttle)
        {
            return false;
        }
        attempts.put(key, now);
        true
    }
}

impl<B: OfficialRecordBackend + ?Sized> OfficialRecordVerifier for CachedOfficialRecordVerifier<B> {
    fn has_official_record<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(self.lookup(account_id, map_uid, None))
    }

    fn has_recent_official_record<'a>(
        &'a self,
        account_id: &'a Uuid,
        map_uid: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(async move {
            for attempt in 0..RECENT_RECORD_ATTEMPTS {
                let negative_not_before = Instant::now();
                if self.lookup(account_id, map_uid, Some(negative_not_before)).await? {
                    return Ok(true);
                }
                if attempt + 1 < RECENT_RECORD_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(250 * (1_u64 << attempt))).await;
                }
            }
            Ok(false)
        })
    }

    fn allow_unlock_validation(&self, account_id: &Uuid, map_uid: &str, hat_slug: &str) -> bool {
        self.allow_unlock_validation_at(account_id, map_uid, hat_slug, Instant::now())
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
        self.get_map_info_optional(map_uid)
            .await?
            .ok_or_else(|| "Nadeo map not found".to_string())
    }

    async fn get_map_info_optional(&self, map_uid: &str) -> Result<Option<LiveMapInfo>, String> {
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
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("map info status {}", resp.status()));
        }
        resp.json::<LiveMapInfo>().await.map(Some).map_err(|e| e.to_string())
    }

    pub async fn author_id_for_uid(&self, map_uid: &str) -> Result<(Uuid, LiveMapInfo), String> {
        let info = self.get_map_info(map_uid).await?;
        let author_id = verified_map_author(map_uid, &info)?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDecision {
    NoRetry,
    RetryAfter(Duration),
    ExceedsBudget,
}

fn retry_delay(status: StatusCode, retry_after: Option<&str>, attempt: usize, now: SystemTime) -> RetryDecision {
    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
        return RetryDecision::NoRetry;
    }
    let requested = retry_after.and_then(|value| {
        value.trim().parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|at| at.duration_since(now).unwrap_or_default())
        })
    });
    if let Some(delay) = requested {
        return if delay > MAX_RETRY_DELAY {
            RetryDecision::ExceedsBudget
        } else {
            RetryDecision::RetryAfter(delay)
        };
    }
    RetryDecision::RetryAfter(Duration::from_millis(200 * (1_u64 << attempt.min(3))).min(MAX_RETRY_DELAY))
}

async fn send_with_retry<F>(mut request: F) -> Result<Response, String>
where
    F: FnMut() -> RequestBuilder,
{
    for attempt in 0..RETRY_ATTEMPTS {
        match request().send().await {
            Ok(response) => {
                let decision = retry_delay(
                    response.status(),
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok()),
                    attempt,
                    SystemTime::now(),
                );
                match decision {
                    RetryDecision::RetryAfter(delay) if attempt + 1 < RETRY_ATTEMPTS => {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    RetryDecision::NoRetry | RetryDecision::RetryAfter(_) | RetryDecision::ExceedsBudget => return Ok(response),
                }
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

fn verified_map_author(requested_uid: &str, info: &LiveMapInfo) -> Result<Uuid, String> {
    if info.uid != requested_uid {
        return Err(format!(
            "Nadeo map info UID mismatch: requested {requested_uid}, received {}",
            info.uid
        ));
    }
    Uuid::parse_str(&info.author).map_err(|error| error.to_string())
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

impl MapAuthorBackend for NadeoLiveClient {
    fn fetch_map_author<'a>(&'a self, map_uid: &'a str) -> Pin<Box<dyn Future<Output = Result<Option<Uuid>, String>> + Send + 'a>> {
        Box::pin(async move {
            let Some(info) = self.get_map_info_optional(map_uid).await? else {
                return Ok(None);
            };
            verified_map_author(map_uid, &info).map(Some)
        })
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

    struct FakeMapAuthorBackend {
        calls: AtomicUsize,
        results: StdMutex<VecDeque<Result<Option<Uuid>, String>>>,
    }

    impl FakeMapAuthorBackend {
        fn new(results: impl IntoIterator<Item = Result<Option<Uuid>, String>>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                results: StdMutex::new(results.into_iter().collect()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl MapAuthorBackend for FakeMapAuthorBackend {
        fn fetch_map_author<'a>(&'a self, _map_uid: &'a str) -> Pin<Box<dyn Future<Output = Result<Option<Uuid>, String>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                self.results
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop_front()
                    .expect("fake map author result")
            })
        }
    }

    struct BlockingMapAuthorBackend {
        calls: AtomicUsize,
        author: Uuid,
        release: tokio::sync::Semaphore,
    }

    impl BlockingMapAuthorBackend {
        fn new(author: Uuid) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                author,
                release: tokio::sync::Semaphore::new(0),
            })
        }
    }

    impl MapAuthorBackend for BlockingMapAuthorBackend {
        fn fetch_map_author<'a>(&'a self, _map_uid: &'a str) -> Pin<Box<dyn Future<Output = Result<Option<Uuid>, String>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let permit = self.release.acquire().await.map_err(|error| error.to_string())?;
                permit.forget();
                Ok(Some(self.author))
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
            Duration::from_secs(60),
            64,
            16,
            8,
        )
    }

    fn cached_records_limited(
        backend: Arc<FakeRecordBackend>,
        capacity: usize,
        global_limit: u32,
        account_limit: u32,
    ) -> CachedOfficialRecordVerifier<FakeRecordBackend> {
        CachedOfficialRecordVerifier::with_config(
            backend,
            capacity,
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(2),
            Duration::from_secs(60),
            global_limit,
            account_limit,
            8,
        )
    }

    fn cached_authors(
        backend: Arc<FakeMapAuthorBackend>,
        capacity: usize,
        global_limit: u32,
        account_limit: u32,
    ) -> CachedMapAuthorVerifier<FakeMapAuthorBackend> {
        CachedMapAuthorVerifier::with_config(
            backend,
            capacity,
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
            global_limit,
            account_limit,
            2,
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

    #[test]
    fn map_author_requires_exact_returned_uid() {
        let author = Uuid::new_v4();
        let requested = "RequestedMap_______________";
        assert_eq!(
            verified_map_author(
                requested,
                &LiveMapInfo {
                    uid: requested.to_string(),
                    author: author.to_string(),
                    name: None,
                    thumbnail_url: None,
                }
            ),
            Ok(author)
        );
        let error = verified_map_author(
            requested,
            &LiveMapInfo {
                uid: "DifferentMap_______________".to_string(),
                author: author.to_string(),
                name: None,
                thumbnail_url: None,
            },
        )
        .expect_err("mismatched UID must fail closed");
        assert!(error.contains("UID mismatch"));
        assert!(error.contains(requested));
        assert!(error.contains("DifferentMap"));
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
    async fn official_record_lookup_is_single_flight_per_account_and_uid() {
        let user = Uuid::new_v4();
        let backend = FakeRecordBackend::new([Ok(true)]);
        let verifier = Arc::new(cached(backend.clone(), 8));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let verifier = verifier.clone();
            tasks.push(tokio::spawn(async move {
                verifier.has_official_record(&user, "SameRecordMap_____________").await
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), Ok(true));
        }
        assert_eq!(backend.calls(), 1);
    }

    #[tokio::test]
    async fn official_record_distinct_uid_fanout_is_globally_and_per_account_bounded() {
        let user = Uuid::new_v4();
        let other = Uuid::new_v4();
        let backend = FakeRecordBackend::new([Ok(true), Ok(true), Ok(true)]);
        let verifier = cached_records_limited(backend.clone(), 8, 3, 2);
        assert!(verifier.has_official_record(&user, "record-1").await.unwrap());
        assert!(verifier.has_official_record(&user, "record-2").await.unwrap());
        assert!(verifier.has_official_record(&user, "record-3").await.is_err());
        assert!(verifier.has_official_record(&other, "record-4").await.unwrap());
        assert!(verifier.has_official_record(&other, "record-5").await.is_err());
        assert!(verifier.has_official_record(&user, "record-1").await.unwrap());
        assert_eq!(backend.calls(), 3);
    }

    #[tokio::test]
    async fn recent_record_refresh_retries_negative_eventual_consistency_within_bound() {
        let user = Uuid::new_v4();
        let backend = FakeRecordBackend::new([Ok(false), Ok(false), Ok(true)]);
        let verifier = cached(backend.clone(), 8);
        assert_eq!(
            verifier.has_recent_official_record(&user, "RecentRecordMap___________").await,
            Ok(true)
        );
        assert_eq!(backend.calls(), RECENT_RECORD_ATTEMPTS);
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
        assert!(verifier.allow_unlock_validation_at(&user, "map-a", "hat-a", start));
        assert!(!verifier.allow_unlock_validation_at(&user, "map-a", "hat-a", start + Duration::from_secs(1)));
        assert!(verifier.allow_unlock_validation_at(&user, "map-a", "hat-b", start + Duration::from_secs(1)));
        assert!(verifier.allow_unlock_validation_at(&other, "map-a", "hat-a", start + Duration::from_secs(1)));
        assert!(verifier.allow_unlock_validation_at(&user, "map-a", "hat-a", start + Duration::from_secs(2)));
    }

    #[test]
    fn retry_policy_honors_delta_and_http_date_without_retrying_early() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(retry_delay(StatusCode::NOT_FOUND, None, 0, now), RetryDecision::NoRetry);
        assert_eq!(
            retry_delay(StatusCode::INTERNAL_SERVER_ERROR, None, 0, now),
            RetryDecision::RetryAfter(Duration::from_millis(200))
        );
        assert_eq!(
            retry_delay(StatusCode::TOO_MANY_REQUESTS, Some("1"), 0, now),
            RetryDecision::RetryAfter(Duration::from_secs(1))
        );
        let soon = httpdate::fmt_http_date(now + Duration::from_secs(2));
        assert_eq!(
            retry_delay(StatusCode::TOO_MANY_REQUESTS, Some(&soon), 0, now),
            RetryDecision::RetryAfter(Duration::from_secs(2))
        );
        let too_late = httpdate::fmt_http_date(now + Duration::from_secs(3));
        assert_eq!(
            retry_delay(StatusCode::TOO_MANY_REQUESTS, Some(&too_late), 0, now),
            RetryDecision::ExceedsBudget
        );
        assert_eq!(
            retry_delay(StatusCode::TOO_MANY_REQUESTS, Some("99"), 0, now),
            RetryDecision::ExceedsBudget
        );
    }

    #[tokio::test]
    async fn map_author_positive_negative_and_transient_cache_policy() {
        let account = Uuid::new_v4();
        let author = Uuid::new_v4();

        let positive_backend = FakeMapAuthorBackend::new([Ok(Some(author))]);
        let positive = cached_authors(positive_backend.clone(), 8, 20, 20);
        assert_eq!(positive.author_id_for_uid(&account, "positive-map").await, Ok(author));
        assert_eq!(positive.author_id_for_uid(&account, "positive-map").await, Ok(author));
        assert_eq!(positive_backend.calls(), 1);

        let negative_backend = FakeMapAuthorBackend::new([Ok(None)]);
        let negative = cached_authors(negative_backend.clone(), 8, 20, 20);
        assert!(negative.author_id_for_uid(&account, "missing-map").await.is_err());
        assert!(negative.author_id_for_uid(&account, "missing-map").await.is_err());
        assert_eq!(negative_backend.calls(), 1);

        let transient_backend = FakeMapAuthorBackend::new([Err("temporary".into()), Ok(Some(author))]);
        let transient = cached_authors(transient_backend.clone(), 8, 20, 20);
        assert_eq!(
            transient.author_id_for_uid(&account, "transient-map").await,
            Err("temporary".into())
        );
        assert_eq!(transient.author_id_for_uid(&account, "transient-map").await, Ok(author));
        assert_eq!(transient_backend.calls(), 2);
    }

    #[tokio::test]
    async fn map_author_uid_mismatch_errors_are_not_cached() {
        let account = Uuid::new_v4();
        let author = Uuid::new_v4();
        let mismatch = verified_map_author(
            "requested-map",
            &LiveMapInfo {
                uid: "returned-map".into(),
                author: author.to_string(),
                name: None,
                thumbnail_url: None,
            },
        )
        .expect_err("fixture must be a mismatch");
        let backend = FakeMapAuthorBackend::new([Err(mismatch.clone()), Err(mismatch.clone())]);
        let verifier = cached_authors(backend.clone(), 8, 20, 20);
        assert_eq!(verifier.author_id_for_uid(&account, "requested-map").await, Err(mismatch.clone()));
        assert_eq!(verifier.author_id_for_uid(&account, "requested-map").await, Err(mismatch));
        assert_eq!(backend.calls(), 2);
    }

    #[tokio::test]
    async fn map_author_lookup_is_single_flight_per_uid() {
        let account = Uuid::new_v4();
        let author = Uuid::new_v4();
        let backend = FakeMapAuthorBackend::new([Ok(Some(author))]);
        let verifier = Arc::new(cached_authors(backend.clone(), 8, 20, 20));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let verifier = verifier.clone();
            tasks.push(tokio::spawn(async move { verifier.author_id_for_uid(&account, "same-map").await }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), Ok(author));
        }
        assert_eq!(backend.calls(), 1);
    }

    #[tokio::test]
    async fn active_single_flight_gates_are_never_evicted_at_capacity() {
        let account = Uuid::new_v4();
        let author = Uuid::new_v4();
        let backend = BlockingMapAuthorBackend::new(author);
        let verifier = Arc::new(CachedMapAuthorVerifier::with_config(
            backend.clone(),
            2,
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
            20,
            20,
            2,
        ));

        let first = {
            let verifier = verifier.clone();
            tokio::spawn(async move { verifier.author_id_for_uid(&account, "map-a").await })
        };
        let second = {
            let verifier = verifier.clone();
            tokio::spawn(async move { verifier.author_id_for_uid(&account, "map-b").await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.calls.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("two lookups reached backend");

        assert_eq!(
            verifier.author_id_for_uid(&account, "map-c").await,
            Err("Nadeo map author single-flight capacity exceeded".to_string())
        );
        let duplicate = {
            let verifier = verifier.clone();
            tokio::spawn(async move { verifier.author_id_for_uid(&account, "map-a").await })
        };
        tokio::task::yield_now().await;
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        backend.release.add_permits(2);
        assert_eq!(first.await.unwrap(), Ok(author));
        assert_eq!(second.await.unwrap(), Ok(author));
        assert_eq!(duplicate.await.unwrap(), Ok(author));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn map_author_arbitrary_uid_fanout_is_admission_bounded() {
        let account = Uuid::new_v4();
        let other = Uuid::new_v4();
        let author = Uuid::new_v4();
        let backend = FakeMapAuthorBackend::new([Ok(Some(author)), Ok(Some(author)), Ok(Some(author))]);
        let verifier = cached_authors(backend.clone(), 8, 3, 2);
        assert!(verifier.author_id_for_uid(&account, "map-1").await.is_ok());
        assert!(verifier.author_id_for_uid(&account, "map-2").await.is_ok());
        assert!(verifier.author_id_for_uid(&account, "map-3").await.is_err());
        assert!(verifier.author_id_for_uid(&other, "map-4").await.is_ok());
        assert!(verifier.author_id_for_uid(&other, "map-5").await.is_err());
        assert_eq!(verifier.author_id_for_uid(&account, "map-1").await, Ok(author));
        assert_eq!(backend.calls(), 3);
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
