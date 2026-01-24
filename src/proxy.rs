//! HTTP proxy client for forwarding requests to Copilot API.

use crate::auth::TokenManager;
use crate::error::Error;
use axum::body::{Body, Bytes};
use axum::response::Response;
use futures::TryStreamExt;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use std::sync::Arc;

const COPILOT_API_BASE: &str = "https://api.individual.githubcopilot.com";

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
}

impl ProxyClient {
    pub fn new(token_manager: Arc<TokenManager>) -> Result<Self, Error> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            token_manager,
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

        let mut req = self
            .client
            .request(method, format!("{}{}", COPILOT_API_BASE, path))
            .bearer_auth(&token)
            .headers(copilot_headers(initiator, is_vision));

        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }

        Ok(req.body(body).send().await?)
    }
}

fn copilot_headers(initiator: Option<&str>, is_vision: bool) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("editor-version", HeaderValue::from_static("vscode/1.98.1"));
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
        HeaderValue::from_static("2025-04-01"),
    );
    h.insert(
        "copilot-integration-id",
        HeaderValue::from_static("vscode-chat"),
    );
    h.insert(
        "openai-intent",
        HeaderValue::from_static("conversation-panel"),
    );

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
