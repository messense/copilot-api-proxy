//! HTTP proxy client for forwarding requests to Copilot API.

use crate::auth::TokenManager;
use crate::error::Error;
use axum::body::{Body, Bytes};
use axum::response::Response;
use futures::TryStreamExt;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use std::sync::Arc;

const HOP_BY_HOP: &[&str] = &[
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "upgrade",
];

pub struct ProxyClient {
    client: Client,
    token_manager: Arc<TokenManager>,
    device_id: String,
    machine_id: String,
    session_id: String,
}

impl ProxyClient {
    pub fn new(token_manager: Arc<TokenManager>) -> Result<Self, Error> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        let device_id = crate::config::load_vscode_device_id();
        let machine_id = crate::config::load_vscode_machine_id();
        let session_id = uuid::Uuid::new_v4().to_string();
        Ok(Self {
            client,
            token_manager,
            device_id,
            machine_id,
            session_id,
        })
    }

    pub async fn forward(
        &self,
        path: &str,
        method: reqwest::Method,
        body: Bytes,
        content_type: Option<&str>,
        initiator: Option<&str>,
        is_vision: bool,
    ) -> Result<reqwest::Response, Error> {
        let token = self.token_manager.get_token().await?;
        let api_base = self.token_manager.get_api_base().await?;

        let resp = self
            .send_request(
                &api_base,
                path,
                method.clone(),
                &body,
                content_type,
                &token,
                initiator,
                is_vision,
            )
            .await?;

        // On 401, force-refresh the Copilot token and retry once (handles sleep/wake expiry)
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::warn!("Received 401 from upstream, attempting token refresh and retry");
            if self.token_manager.force_refresh(&token).await.is_ok() {
                let new_token = self.token_manager.get_token().await?;
                let new_api_base = self.token_manager.get_api_base().await?;
                return self
                    .send_request(
                        &new_api_base,
                        path,
                        method,
                        &body,
                        content_type,
                        &new_token,
                        initiator,
                        is_vision,
                    )
                    .await;
            }
        }

        Ok(resp)
    }

    pub async fn fetch_usage(&self) -> Result<reqwest::Response, Error> {
        let github_token = &self.token_manager.github_token;
        let resp = self
            .client
            .get("https://api.github.com/copilot_internal/user")
            .header("Authorization", format!("token {}", github_token))
            .header("Accept", "application/json")
            .header("editor-version", "vscode/1.114.0")
            .header("editor-plugin-version", "copilot-chat/0.26.7")
            .header("user-agent", "GitHubCopilotChat/0.26.7")
            .header("x-github-api-version", "2026-01-09")
            .send()
            .await?;
        Ok(resp)
    }

    async fn send_request(
        &self,
        api_base: &str,
        path: &str,
        method: reqwest::Method,
        body: &Bytes,
        content_type: Option<&str>,
        token: &str,
        initiator: Option<&str>,
        is_vision: bool,
    ) -> Result<reqwest::Response, Error> {
        let resolved_initiator = if initiator == Some("agent") {
            "agent"
        } else {
            "user"
        };
        crate::server::record_upstream(resolved_initiator, path);

        let mut req = self
            .client
            .request(method, format!("{}{}", api_base, path))
            .bearer_auth(token)
            .headers(copilot_headers(
                &self.device_id,
                &self.machine_id,
                &self.session_id,
                initiator,
                is_vision,
            ));

        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }

        Ok(req.body(body.clone()).send().await?)
    }
}

fn copilot_headers(
    device_id: &str,
    machine_id: &str,
    session_id: &str,
    initiator: Option<&str>,
    is_vision: bool,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("editor-version", HeaderValue::from_static("vscode/1.114.0"));
    h.insert(
        "editor-plugin-version",
        HeaderValue::from_static("copilot-chat/0.26.7"),
    );
    h.insert(
        "user-agent",
        HeaderValue::from_static("GitHubCopilotChat/0.26.7"),
    );
    h.insert(
        "x-github-api-version",
        HeaderValue::from_static("2026-01-09"),
    );
    h.insert(
        "copilot-integration-id",
        HeaderValue::from_static("vscode-chat"),
    );
    h.insert(
        "openai-intent",
        HeaderValue::from_static("conversation-agent"),
    );
    if let Ok(val) = HeaderValue::from_str(device_id) {
        h.insert("editor-device-id", val);
    }

    // Per-request unique ID, matching the real extension behavior
    if let Ok(val) = HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()) {
        h.insert("x-request-id", val);
    }

    // Session context headers sent by the real VSCode Copilot extension.
    // These are used by the API for rate-limit bucketing and telemetry.
    if let Ok(val) = HeaderValue::from_str(machine_id) {
        h.insert("vscode-machineid", val);
    }
    if let Ok(val) = HeaderValue::from_str(session_id) {
        h.insert("vscode-sessionid", val);
    }

    // X-Initiator: "user" consumes premium, "agent" does not
    h.insert(
        "X-Initiator",
        HeaderValue::from_static(if initiator == Some("agent") {
            "agent"
        } else {
            "user"
        }),
    );

    if is_vision {
        h.insert("Copilot-Vision-Request", HeaderValue::from_static("true"));
    }

    h
}

/// Forward upstream response to client
pub async fn forward_response(resp: reqwest::Response) -> Result<Response, Error> {
    let status = resp.status();
    let headers = resp.headers().clone();

    let is_stream = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    let mut builder = Response::builder().status(status);

    for (key, value) in headers.iter() {
        if !HOP_BY_HOP.contains(&key.as_str()) {
            builder = builder.header(key, value);
        }
    }

    if is_stream && !headers.contains_key("cache-control") {
        builder = builder.header("Cache-Control", "no-cache");
    }

    let body = if is_stream {
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        Body::from_stream(stream)
    } else {
        Body::from(resp.bytes().await?)
    };

    builder
        .body(body)
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}
