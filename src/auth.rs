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
    /// Map of endpoint labels to their base URLs.
    /// The key "api" holds the Copilot API base URL, which varies by plan
    /// (individual, business, enterprise).
    #[serde(default)]
    endpoints: std::collections::HashMap<String, String>,
}

async fn exchange_token(github_token: &str) -> Result<TokenExchangeResponse, Error> {
    reqwest::Client::new()
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("token {}", github_token))
        .header("Accept", "application/json")
        .header("editor-version", "vscode/1.114.0")
        .header("editor-plugin-version", "copilot-chat/0.26.7")
        .header("user-agent", "GitHubCopilotChat/0.26.7")
        .header("x-github-api-version", "2026-01-09")
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
    api_base: String,
    refresh_at: std::time::SystemTime,
}

/// Thread-safe token manager with background refresh
pub struct TokenManager {
    pub(crate) github_token: String,
    copilot_token: Arc<RwLock<Option<CopilotToken>>>,
    refresh_handle: JoinHandle<()>,
}

impl TokenManager {
    pub async fn new(github_token: String) -> Result<Self, Error> {
        let initial = Self::fetch_token(&github_token).await?;
        let copilot_token = Arc::new(RwLock::new(Some(initial)));
        let refresh_handle = Self::spawn_refresh(github_token.clone(), Arc::clone(&copilot_token));

        Ok(Self {
            github_token,
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

    pub async fn get_api_base(&self) -> Result<String, Error> {
        self.copilot_token
            .read()
            .await
            .as_ref()
            .map(|t| t.api_base.clone())
            .ok_or_else(|| Error::Auth("No token available".into()))
    }

    /// Force an immediate token refresh (e.g. after receiving a 401 from upstream).
    ///
    /// The caller passes the `stale_token` that triggered the 401. Holding the
    /// write lock for the duration prevents concurrent refreshes — if another
    /// caller already refreshed, the token won't match and we skip.
    pub async fn force_refresh(&self, stale_token: &str) -> Result<(), Error> {
        let mut guard = self.copilot_token.write().await;

        // Another caller may have already refreshed while we waited for the lock.
        if let Some(current) = guard.as_ref() {
            if current.token != stale_token {
                tracing::debug!("Token already refreshed by another request, skipping");
                return Ok(());
            }
        }

        tracing::info!("Forcing Copilot token refresh");
        let new = Self::fetch_token(&self.github_token).await?;
        *guard = Some(new);
        tracing::info!("Forced Copilot token refresh succeeded");
        Ok(())
    }

    async fn fetch_token(github_token: &str) -> Result<CopilotToken, Error> {
        let resp = exchange_token(github_token).await?;
        let secs = resp.refresh_in.saturating_sub(60).max(1) as u64;

        // Extract API base URL from endpoints map, falling back to individual
        let api_base = match resp.endpoints.get("api") {
            Some(url) => {
                let url = url.trim_end_matches('/').to_string();
                tracing::info!("Copilot API base: {}", url);
                url
            }
            None => {
                let url = "https://api.individual.githubcopilot.com".to_string();
                tracing::warn!(
                    "Token exchange response missing 'api' endpoint, \
                     falling back to {}",
                    url
                );
                url
            }
        };

        Ok(CopilotToken {
            token: resp.token,
            api_base,
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
