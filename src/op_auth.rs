use lazy_static::lazy_static;
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::{collections::HashMap, error::Error, num::NonZeroUsize};
use tokio::sync::RwLock;
use tokio::{fs::File, io::AsyncReadExt};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub secret: String,
    pub url: String,
    pub plugin_id: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TokenResp {
    pub account_id: String,
    pub display_name: String,
    pub token_time: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JErrorResp {
    pub error: String,
}

lazy_static! {
    static ref PLUGIN_SITE_ID_TO_OP_CONFIG: RwLock<HashMap<i32, Config>> = RwLock::new(HashMap::new());
    static ref TOKEN_CACHE: RwLock<LruCache<String, TokenResp>> = RwLock::new(LruCache::new(NonZeroUsize::new(4096).unwrap()));
}

pub async fn init_op_config() -> usize {
    let mut configs = PLUGIN_SITE_ID_TO_OP_CONFIG.write().await;

    let file = File::open("./config.json").await;
    let res: Result<(), Box<dyn Error>> = async {
        let mut f = file?;
        let mut contents = String::new();
        f.read_to_string(&mut contents).await?;
        let config: Config = serde_json::from_str(&contents)?;
        configs.insert(config.plugin_id, config);
        Ok(())
    }
    .await;

    if let Err(e) = res {
        log::error!("Error reading config file: {:?}", e);
    }

    configs.len()
}

pub async fn check_token(token: &str, plugin_id: i32) -> Option<TokenResp> {
    #[cfg(test)]
    {
        if token == "test_token" {
            return Some(TokenResp {
                account_id: "0a2d1bc0-4aaa-4374-b2db-3d561bdab1c9".to_string(),
                display_name: "XertroV".to_string(),
                token_time: 1234,
            });
        }
    }

    let configs = PLUGIN_SITE_ID_TO_OP_CONFIG.read().await;
    let config = match configs.get(&plugin_id) {
        Some(config) => config,
        None => {
            log::error!("Bad plugin_id: {}", plugin_id);
            return None;
        }
    };

    // This is a placeholder for your get_token_cached_and_recent function.
    // Implement cache check here...
    let mut token_cache = TOKEN_CACHE.write().await;
    if let Some(token_resp) = token_cache.get(token) {
        return Some(token_resp.clone());
        // if token_resp.token_time > (chrono::Utc::now().timestamp() - 3600) {
        // }
    }
    drop(token_cache);

    let client = reqwest::Client::new();
    let res = client
        .post(&config.url)
        .form(&[("token", token), ("secret", &config.secret)])
        .send()
        .await;

    match res {
        Ok(response) => {
            if response.status() != reqwest::StatusCode::OK {
                log::warn!("Checking token failed, status: {}", response.status());
                return None;
            }
            log::info!("Token check request succeeded successfully.");
            let resp_text = response.bytes().await.unwrap_or_default();
            match serde_json::from_slice::<TokenResp>(&resp_text) {
                Ok(resp_j) => {
                    let mut token_cache = TOKEN_CACHE.write().await;
                    token_cache.put(token.to_string(), resp_j.clone());
                    Some(resp_j)
                }
                Err(_) => {
                    if let Ok(j_error) = serde_json::from_slice::<JErrorResp>(&resp_text) {
                        log::warn!("Error response from server: {:?}", j_error.error);
                    } else {
                        log::warn!("Error parsing response JSON. Original: {:?}", resp_text);
                    }
                    None
                }
            }
        }
        Err(_) => {
            log::warn!("Failed to send request.");
            None
        }
    }
}
