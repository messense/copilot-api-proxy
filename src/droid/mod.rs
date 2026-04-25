//! Droid CLI integration: local LLM routing plus optional local control plane.

pub mod local;

use crate::error::Error;
use crate::llm;
use crate::proxy::forward_response;
use crate::server::AppState;
use axum::Json;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use reqwest::Client;

const FACTORY_DEFAULT_UPSTREAM: &str = "https://api.factory.ai";
const SKIP_FORWARD_HEADERS: &[&str] = &[
    "host",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "upgrade",
];

/// Reverse proxy for Droid management and control-plane routes.
pub struct DroidManagementProxy {
    client: Client,
    upstream_url: String,
}

impl Default for DroidManagementProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl DroidManagementProxy {
    pub fn new() -> Self {
        Self::with_upstream_url(
            std::env::var("FACTORY_UPSTREAM_URL")
                .or_else(|_| std::env::var("DROID_UPSTREAM_URL"))
                .unwrap_or_else(|_| FACTORY_DEFAULT_UPSTREAM.to_string())
                .trim_end_matches('/')
                .to_string(),
        )
    }

    fn with_upstream_url(upstream_url: String) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to create HTTP client for droid management proxy");
        Self {
            client,
            upstream_url,
        }
    }

    pub async fn forward(
        &self,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response, Error> {
        let url = format!("{}{}", self.upstream_url, path_and_query);
        let mut req = self.client.request(method, &url);
        for (key, value) in headers.iter() {
            if !SKIP_FORWARD_HEADERS.contains(&key.as_str()) {
                req = req.header(key, value);
            }
        }
        let resp = req.body(body).send().await?;
        forward_response(resp).await
    }
}

/// Top-level `/api/<segment>` segments that the Droid CLI talks to.
///
/// Anything matched here is owned by the Droid branch (proxied to
/// `FACTORY_UPSTREAM_URL` by default, served locally or 501'd under
/// `--droid-local`). It must NEVER fall through to the Amp branch and hit
/// `ampcode.com`.
///
/// Inventory verified against the `droid` CLI binary (v0.109.1):
///   - `cli/whoami`
///   - `feature-flags`
///   - `organization/managed-settings`, `organization/agent-readiness-reports`
///   - `sessions/create`, `sessions/{id}` and its subpaths
///     (`update-settings`, `update-title`, `message/create`, `droid-status`,
///     `archive`, `unarchive`, `privacy`, `git-ai/checkpoints`)
///   - `llm/o/v1/*`, `llm/a/v1/*`, `llm/g/v1/generate`,
///     `llm/custom/usage`, `llm/failed-requests`
///   - `daemon/heartbeat`
///   - `hello`
///   - `ingest`, `otlp/traces/ingest`         (telemetry; note: NOT under `/api/telemetry/`)
///   - `integrations/org/check`
///   - `tools/web-search`, `tools/get-url-contents`, `tools/slack/post-message`
///   - `v0/computers[...]`, `v0/automations[...]`
pub fn matches_api_path(path: &str) -> bool {
    let head = path.split('/').next().unwrap_or(path);
    matches!(
        head,
        "cli"
            | "feature-flags"
            | "organization"
            | "sessions"
            | "llm"
            | "telemetry"
            | "daemon"
            | "hello"
            | "ingest"
            | "otlp"
            | "integrations"
            | "tools"
            | "v0"
    )
}

pub async fn handle_api_request(
    state: AppState,
    method: Method,
    path: &str,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    if let Some(rest) = path.strip_prefix("llm/") {
        return handle_llm_request(state, method, rest, uri, headers, body).await;
    }

    if let Some(ref local_state) = state.droid_local {
        return local::handle_local_api(local_state, &method, path, &body).await;
    }

    let pq = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());
    state
        .droid_management
        .forward(method, pq, headers, body)
        .await
}

async fn handle_llm_request(
    state: AppState,
    method: Method,
    llm_path: &str,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    let pq = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());

    if matches!(llm_path, "custom/usage" | "failed-requests") {
        if state.droid_local.is_some() {
            return Ok(Json(serde_json::json!({ "ok": true })).into_response());
        }
        return state
            .droid_management
            .forward(method, pq, headers, body)
            .await;
    }

    if let Some(openai_path) = llm_path.strip_prefix("o/v1/") {
        return llm::handle_openai_passthrough(
            &state,
            method,
            openai_path,
            uri.query(),
            &headers,
            body,
        )
        .await;
    }

    if let Some(anthropic_path) = llm_path.strip_prefix("a/v1/") {
        return llm::handle_anthropic_compat(
            &state,
            method,
            anthropic_path,
            uri.query(),
            &headers,
            body,
            false,
        )
        .await;
    }

    if llm_path == "g/v1/generate" {
        let model = match llm::extract_model_field(&body) {
            Ok(model) => model,
            Err(err) => return Ok(crate::claude::error_from_proxy(err)),
        };
        return llm::handle_gemini_generate_content(
            &state,
            method,
            &model,
            uri.query(),
            body,
            true,
        )
        .await;
    }

    if state.droid_local.is_some() {
        return Ok(droid_local_fallback_response(
            &method,
            pq,
            format!("unsupported local llm path '{llm_path}'"),
        ));
    }

    state
        .droid_management
        .forward(method, pq, headers, body)
        .await
}

fn droid_local_fallback_response(
    method: &Method,
    path: &str,
    reason: impl Into<String>,
) -> Response {
    let reason = reason.into();
    tracing::error!(
        target: "droid_proxy",
        method = %method,
        path = %path,
        reason = %reason,
        "droid-local blocked upstream fallback"
    );
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": {
                "message": format!(
                    "--droid-local blocked upstream fallback for {} {}: {}",
                    method, path, reason
                ),
                "type": "droid_local_unimplemented",
                "path": path,
                "method": method.as_str()
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::matches_api_path;

    #[test]
    fn matches_control_plane_paths() {
        // Confirmed against droid CLI binary v0.109.1.
        assert!(matches_api_path("sessions"));
        assert!(matches_api_path("cli/whoami"));
        assert!(matches_api_path("feature-flags"));
        assert!(matches_api_path("organization/managed-settings"));
        assert!(matches_api_path("organization/agent-readiness-reports"));
        assert!(matches_api_path("sessions/create"));
        assert!(matches_api_path("sessions/abc"));
        assert!(matches_api_path("sessions/abc/archive"));
        assert!(matches_api_path("sessions/abc/git-ai/checkpoints"));
        assert!(matches_api_path("llm/o/v1/responses"));
        assert!(matches_api_path("llm/a/v1/messages"));
        assert!(matches_api_path("llm/g/v1/generate"));
        assert!(matches_api_path("llm/custom/usage"));
        assert!(matches_api_path("llm/failed-requests"));
        assert!(matches_api_path("daemon/heartbeat"));
        assert!(matches_api_path("hello"));
        assert!(matches_api_path("ingest"));
        assert!(matches_api_path("otlp/traces/ingest"));
        assert!(matches_api_path("integrations/org/check"));
        assert!(matches_api_path("tools/web-search"));
        assert!(matches_api_path("v0/computers"));
        assert!(matches_api_path("v0/automations"));
        // Older alias still kept for compatibility.
        assert!(matches_api_path("telemetry/cli-ingest"));
    }

    #[test]
    fn excludes_amp_paths() {
        assert!(!matches_api_path("threads/find"));
        assert!(!matches_api_path("internal"));
        assert!(!matches_api_path("provider/openai/v1/responses"));
        assert!(!matches_api_path("news.rss"));
    }
}
