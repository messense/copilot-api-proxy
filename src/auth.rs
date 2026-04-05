//! Authentication: GitHub OAuth device flow, token exchange, and lifecycle management.

use crate::error::Error;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

// ============================================================================
// Device Flow
// ============================================================================

/// Run the GitHub OAuth Device Flow
pub async fn run_device_flow() -> Result<String, Error> {
    let device = get_device_code().await?;

    println!();
    println!("Please authenticate with GitHub:");
    println!("  1. Go to: {}", device.verification_uri);
    println!("  2. Enter code: {}", device.user_code);
    println!();

    poll_for_token(&device.device_code, device.interval).await
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
}

async fn get_device_code() -> Result<DeviceCodeResponse, Error> {
    reqwest::Client::new()
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[("client_id", CLIENT_ID)])
        .send()
        .await?
        .json()
        .await
        .map_err(|e| Error::Auth(format!("Failed to parse device code: {}", e)))
}

async fn poll_for_token(device_code: &str, interval: u64) -> Result<String, Error> {
    let client = reqwest::Client::new();
    let mut current_interval = interval;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(current_interval)).await;

        #[derive(Deserialize)]
        struct Resp {
            access_token: Option<String>,
            error: Option<String>,
        }

        let resp: Resp = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?
            .json()
            .await
            .map_err(|e| Error::Auth(format!("Failed to parse token: {}", e)))?;

        if let Some(token) = resp.access_token {
            return Ok(token);
        }

        match resp.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                // GitHub wants us to poll less frequently - increase interval by 5 seconds
                current_interval += 5;
                continue;
            }
            Some(e) => return Err(Error::Auth(format!("Authorization failed: {}", e))),
            None => continue,
        }
    }
}

// ============================================================================
// Token Exchange
// ============================================================================

#[derive(Deserialize)]
struct TokenExchangeResponse {
    token: String,
    refresh_in: i64,
}

async fn exchange_token(github_token: &str) -> Result<TokenExchangeResponse, Error> {
    reqwest::Client::new()
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("token {}", github_token))
        .header("Accept", "application/json")
        .header("editor-version", "vscode/1.98.1")
        .header("editor-plugin-version", "copilot-chat/0.26.7")
        .header("user-agent", "GitHubCopilotChat/0.26.7")
        .header("x-github-api-version", "2025-04-01")
        .send()
        .await?
        .json()
        .await
        .map_err(|e| Error::Auth(format!("Token exchange failed: {}", e)))
}

// ============================================================================
// Token Manager
// ============================================================================

struct CopilotToken {
    token: String,
    refresh_at: std::time::SystemTime,
}

/// Thread-safe token manager with background refresh
pub struct TokenManager {
    copilot_token: Arc<RwLock<Option<CopilotToken>>>,
    refresh_handle: JoinHandle<()>,
}

impl TokenManager {
    pub async fn new(github_token: String) -> Result<Self, Error> {
        let initial = Self::fetch_token(&github_token).await?;
        let copilot_token = Arc::new(RwLock::new(Some(initial)));
        let refresh_handle = Self::spawn_refresh(github_token, Arc::clone(&copilot_token));

        Ok(Self {
            copilot_token,
            refresh_handle,
        })
    }

    pub async fn get_token(&self) -> Result<String, Error> {
        self.copilot_token
            .read()
            .await
            .as_ref()
            .map(|t| t.token.clone())
            .ok_or_else(|| Error::Auth("No token available".into()))
    }

    async fn fetch_token(github_token: &str) -> Result<CopilotToken, Error> {
        let resp = exchange_token(github_token).await?;
        let secs = resp.refresh_in.saturating_sub(60).max(1) as u64;
        Ok(CopilotToken {
            token: resp.token,
            refresh_at: std::time::SystemTime::now() + std::time::Duration::from_secs(secs),
        })
    }

    fn spawn_refresh(
        github_token: String,
        token: Arc<RwLock<Option<CopilotToken>>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let refresh_at = match token.read().await.as_ref() {
                    Some(t) => t.refresh_at,
                    None => break,
                };

                // Use wall-clock time to handle sleep/wake correctly
                let now = std::time::SystemTime::now();
                if let Ok(duration) = refresh_at.duration_since(now) {
                    tokio::time::sleep(duration).await;
                }
                // If refresh_at is in the past (e.g., after sleep), refresh immediately

                match Self::fetch_token(&github_token).await {
                    Ok(new) => {
                        tracing::info!("Refreshed Copilot token");
                        *token.write().await = Some(new);
                    }
                    Err(e) => {
                        tracing::error!("Token refresh failed: {}", e);
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                }
            }
        })
    }
}

impl Drop for TokenManager {
    fn drop(&mut self) {
        self.refresh_handle.abort();
    }
}
