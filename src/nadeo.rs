//! Minimal Nadeo Live Services client for lazy map author lookup.
//! Auth: dedicated-server basic auth (preferred) or skip if unset.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use log::{info, warn};
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct LiveMapInfo {
    pub uid: String,
    pub author: String,
    pub name: Option<String>,
    #[serde(rename = "thumbnailUrl")]
    pub thumbnail_url: Option<String>,
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

pub struct NadeoLiveClient {
    http: reqwest::Client,
    user_agent: String,
    login: Option<String>,
    password: Option<String>,
    token: RwLock<Option<CachedToken>>,
}

impl NadeoLiveClient {
    pub fn from_env() -> Arc<Self> {
        let login = std::env::var("DD2_NADEO_DS_USERNAME").ok().filter(|s| !s.is_empty());
        let password = std::env::var("DD2_NADEO_DS_PASSWORD").ok().filter(|s| !s.is_empty());
        let email = std::env::var("DD2_NADEO_UA_EMAIL").unwrap_or_else(|_| "dd2-server@xk.io".to_string());
        let user_agent = format!("dd2-server / Max Kaye / {}", email);
        if login.is_none() || password.is_none() {
            warn!("DD2_NADEO_DS_USERNAME/PASSWORD not set; map author lazy-fetch disabled");
        } else {
            info!("Nadeo Live client configured (dedicated server auth)");
        }
        Arc::new(Self {
            http: reqwest::Client::new(),
            user_agent,
            login,
            password,
            token: RwLock::new(None),
        })
    }

    pub fn is_configured(&self) -> bool {
        self.login.is_some() && self.password.is_some()
    }

    async fn ensure_token(&self) -> Result<String, String> {
        {
            let guard = self.token.read().await;
            if let Some(t) = guard.as_ref() {
                if t.obtained.elapsed() < Duration::from_secs(50 * 60) {
                    return Ok(t.access.clone());
                }
            }
        }
        self.authenticate().await
    }

    async fn authenticate(&self) -> Result<String, String> {
        let login = self.login.as_ref().ok_or("nadeo not configured")?;
        let password = self.password.as_ref().ok_or("nadeo not configured")?;
        let basic = base64::prelude::BASE64_STANDARD.encode(format!("{}:{}", login, password));
        let resp = self
            .http
            .post("https://prod.trackmania.core.nadeo.online/v2/authentication/token/basic")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Basic {}", basic))
            .header("User-Agent", &self.user_agent)
            .json(&serde_json::json!({ "audience": "NadeoLiveServices" }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("nadeo auth status {}", resp.status()));
        }
        let body: TokenResp = resp.json().await.map_err(|e| e.to_string())?;
        let access = body.access_token.clone();
        *self.token.write().await = Some(CachedToken {
            access: body.access_token,
            refresh: body.refresh_token,
            obtained: Instant::now(),
        });
        Ok(access)
    }

    pub async fn get_map_info(&self, map_uid: &str) -> Result<LiveMapInfo, String> {
        if !self.is_configured() {
            return Err("nadeo not configured".into());
        }
        let token = self.ensure_token().await?;
        let url = format!("https://live-services.trackmania.nadeo.live/api/token/map/{}", map_uid);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("nadeo_v1 t={}", token))
            .header("User-Agent", &self.user_agent)
            .send()
            .await
            .map_err(|e| e.to_string())?;
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
}
