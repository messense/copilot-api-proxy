//! Droid CLI integration: local LLM routing plus optional local control plane.

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

pub fn matches_api_path(path: &str) -> bool {
    path == "cli"
        || path.starts_with("cli/")
        || path == "feature-flags"
        || path == "organization"
        || path.starts_with("organization/")
        || path == "sessions"
        || path.starts_with("sessions/")
        || path == "llm"
        || path.starts_with("llm/")
        || path == "telemetry"
        || path.starts_with("telemetry/")
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
        return crate::droid_local::handle_local_api(local_state, &method, path, &body).await;
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
        assert!(matches_api_path("sessions"));
        assert!(matches_api_path("cli/whoami"));
        assert!(matches_api_path("feature-flags"));
        assert!(matches_api_path("organization/managed-settings"));
        assert!(matches_api_path("sessions/create"));
        assert!(matches_api_path("llm/o/v1/responses"));
        assert!(matches_api_path("telemetry/cli-ingest"));
    }

    #[test]
    fn excludes_amp_paths() {
        assert!(!matches_api_path("threads/find"));
        assert!(!matches_api_path("internal"));
        assert!(!matches_api_path("provider/openai/v1/responses"));
    }
}
