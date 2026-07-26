//! Trackmania OAuth (api.trackmania.com) — authorization code flow.
//! Docs: https://webservices.openplanet.dev/oauth/auth

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TmOauthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

impl TmOauthConfig {
    pub fn from_env() -> Option<Self> {
        let client_id = std::env::var("TM_OAUTH_CLIENT_ID").ok()?;
        let client_secret = std::env::var("TM_OAUTH_CLIENT_SECRET").ok()?;
        let redirect_uri = std::env::var("TM_OAUTH_REDIRECT_URI").ok()?;
        if client_id.is_empty() || client_secret.is_empty() || redirect_uri.is_empty() {
            return None;
        }
        Some(Self {
            client_id,
            client_secret,
            redirect_uri,
        })
    }

    pub fn authorize_url(&self, state: &str) -> String {
        format!(
            "https://api.trackmania.com/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&state={}",
            urlencoding_encode(&self.client_id),
            urlencoding_encode(&self.redirect_uri),
            urlencoding_encode(state)
        )
    }
}

fn urlencoding_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub token_type: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TmUser {
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
}

impl TmUser {
    pub fn account_uuid(&self) -> Result<Uuid, uuid::Error> {
        Uuid::parse_str(&self.account_id)
    }
}

pub async fn exchange_code(cfg: &TmOauthConfig, code: &str) -> Result<TokenResponse, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.trackmania.com/api/access_token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", cfg.redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token exchange {}: {}", status, body));
    }
    resp.json().await.map_err(|e| e.to_string())
}

pub async fn fetch_user(access_token: &str) -> Result<TmUser, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.trackmania.com/api/user")
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("api/user status {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

pub async fn display_names(access_token: &str, account_ids: &[Uuid]) -> Result<std::collections::HashMap<String, String>, String> {
    if account_ids.is_empty() {
        return Ok(Default::default());
    }
    let mut url = "https://api.trackmania.com/api/display-names?".to_string();
    for (i, id) in account_ids.iter().enumerate() {
        if i > 0 {
            url.push('&');
        }
        url.push_str(&format!("accountId[]={}", id));
    }
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("display-names status {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

pub async fn account_ids_for_names(access_token: &str, names: &[&str]) -> Result<std::collections::HashMap<String, String>, String> {
    if names.is_empty() {
        return Ok(Default::default());
    }
    let mut url = "https://api.trackmania.com/api/display-names/account-ids?".to_string();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            url.push('&');
        }
        url.push_str(&format!("displayName[]={}", urlencoding_encode(name)));
    }
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("account-ids status {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}
