//! Amp CLI/IDE integration: provider routing and management proxy.
//!
//! Amp CLI (ampcode.com) sends two kinds of requests:
//! 1. Provider API requests: `/api/provider/{provider}/v1/...` (inference)
//! 2. Management requests: `/api/auth/*`, `/api/threads/*`, `/threads/*`, etc.
//!
//! Provider requests are handled locally through Copilot (OpenAI + Anthropic).
//! Unknown providers and management requests are proxied to ampcode.com.

use crate::claude::{analyze_claude_request, convert_claude_request, error_from_proxy};
use crate::error::Error;
use crate::gemini::{handle_gemini_count_tokens, parse_gemini_action};
use crate::llm;
use crate::proxy::forward_response;
use crate::server::AppState;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::any;
use reqwest::Client;

// ---------------------------------------------------------------------------
// Management proxy to ampcode.com
// ---------------------------------------------------------------------------

const AMPCODE_DEFAULT_UPSTREAM: &str = "https://ampcode.com";

/// Hop-by-hop headers that should NOT be forwarded to ampcode.com upstream.
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

/// Reverse proxy for Amp management routes (auth, threads, user, etc.).
pub struct AmpManagementProxy {
    client: Client,
    upstream_url: String,
}

impl Default for AmpManagementProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl AmpManagementProxy {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to create HTTP client for amp management proxy");
        let upstream_url = std::env::var("AMP_UPSTREAM_URL")
            .unwrap_or_else(|_| AMPCODE_DEFAULT_UPSTREAM.to_string())
            .trim_end_matches('/')
            .to_string();
        Self {
            client,
            upstream_url,
        }
    }

    /// Forward a request to ampcode.com.
    /// `path_and_query` should include the leading `/` and optional `?query`.
    pub async fn forward(
        &self,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response, Error> {
        let url = format!("{}{}", self.upstream_url, path_and_query);

        // Log the request details
        tracing::debug!(
            target: "amp_proxy",
            method = %method,
            path = %path_and_query,
            body_size = body.len(),
            "Forwarding request to ampcode.com"
        );

        // Log request body if it's small and looks like JSON
        if body.len() > 0 && body.len() < 4096 {
            if let Ok(body_str) = std::str::from_utf8(&body) {
                if body_str.trim_start().starts_with('{') || body_str.trim_start().starts_with('[')
                {
                    tracing::debug!(
                        target: "amp_proxy",
                        body = %body_str,
                        "Request body"
                    );
                }
            }
        }

        let mut req = self.client.request(method.clone(), &url);

        // Resolve upstream API key: AMP_API_KEY env → amp secrets file
        let upstream_key = resolve_ampcode_api_key();

        // Forward headers, stripping hop-by-hop headers.
        // Also strip auth headers when we have a resolved upstream key
        // (we'll inject the resolved key instead).
        for (key, value) in headers.iter() {
            let k = key.as_str();
            if SKIP_FORWARD_HEADERS.contains(&k) {
                continue;
            }
            if upstream_key.is_some()
                && (k == "authorization" || k == "x-api-key" || k == "x-goog-api-key")
            {
                continue;
            }
            req = req.header(key, value);
        }

        // Inject resolved upstream key if available
        if let Some(api_key) = &upstream_key {
            req = req.header("authorization", format!("Bearer {api_key}"));
            req = req.header("x-api-key", api_key);
        }

        let resp = req.body(body).send().await?;

        // Log the response details
        tracing::debug!(
            target: "amp_proxy",
            method = %method,
            path = %path_and_query,
            status = %resp.status(),
            "Received response from ampcode.com"
        );

        forward_response(resp).await
    }
}

/// Resolve the ampcode.com API key from available sources.
/// Priority: `AMP_API_KEY` env var → `~/.local/share/amp/secrets.json`
fn resolve_ampcode_api_key() -> Option<String> {
    // 1. Environment variable
    if let Ok(key) = std::env::var("AMP_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }

    // 2. Amp secrets file (written by `amp login`)
    let home = dirs::home_dir()?;
    let secrets_path = home
        .join(".local")
        .join("share")
        .join("amp")
        .join("secrets.json");
    let content = std::fs::read_to_string(secrets_path).ok()?;
    let secrets: serde_json::Value = serde_json::from_str(&content).ok()?;
    secrets
        .get("apiKey@https://ampcode.com/")
        .or_else(|| secrets.get("apiKey@https://ampcode.com"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn amp_local_fallback_response(method: &Method, path: &str, reason: impl Into<String>) -> Response {
    let reason = reason.into();
    tracing::error!(
        target: "amp_proxy",
        method = %method,
        path = %path,
        reason = %reason,
        "amp-local blocked upstream fallback"
    );
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": {
                "message": format!(
                    "--amp-local blocked upstream fallback for {} {}: {}",
                    method, path, reason
                ),
                "type": "amp_local_unimplemented",
                "path": path,
                "method": method.as_str()
            }
        })),
    )
        .into_response()
}

fn amp_local_stub_news_rss() -> Response {
    tracing::warn!(
        target: "amp_proxy",
        path = "/news.rss",
        "Serving local stub for /news.rss in --amp-local mode"
    );
    (
        [
            (header::CONTENT_TYPE, "application/rss+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Amp Local News</title>
    <description>Local stub feed served by copilot-api-proxy in --amp-local mode.</description>
    <link>http://localhost/</link>
  </channel>
</rss>
"#,
    )
        .into_response()
}

fn is_browser_route(path: &str) -> bool {
    matches!(path, "/auth" | "/threads" | "/docs" | "/settings")
        || path.starts_with("/auth/")
        || path.starts_with("/threads/")
        || path.starts_with("/docs/")
        || path.starts_with("/settings/")
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Create root-level Amp routes to merge into the main router.
pub fn root_routes() -> Router<AppState> {
    Router::new()
        // Root-level management routes that Amp CLI expects
        .route("/threads", any(management_handler))
        .route("/threads/{*path}", any(management_handler))
        .route("/threads.rss", any(management_handler))
        .route("/news.rss", any(management_handler))
        .route("/auth", any(management_handler))
        .route("/auth/{*path}", any(management_handler))
        .route("/docs", any(management_handler))
        .route("/docs/{*path}", any(management_handler))
        .route("/settings", any(management_handler))
        .route("/settings/{*path}", any(management_handler))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Dispatch Amp `/api/*` requests: provider routes go to Copilot, rest to ampcode.com.
pub async fn handle_api_request(
    state: AppState,
    method: Method,
    path: &str,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    tracing::debug!(
        target: "amp_proxy",
        method = %method,
        path = %path,
        "Received /api/* request"
    );

    if let Some(rest) = path.strip_prefix("provider/")
        && let Some((provider, provider_path)) = rest.split_once('/')
    {
        tracing::debug!(
            target: "amp_proxy",
            provider = %provider,
            provider_path = %provider_path,
            "Routing to provider handler"
        );
        return handle_provider(state, method, provider, provider_path, &uri, headers, body).await;
    }

    // When amp-local mode is enabled, handle management routes locally
    if let Some(ref local_state) = state.amp_local
        && crate::amp_local::should_handle_locally(&path)
    {
        tracing::debug!(
            target: "amp_proxy",
            path = %path,
            "Handling locally with amp-local"
        );
        return crate::amp_local::handle_local_api(
            local_state,
            &method,
            &path,
            uri.query(),
            &headers,
            &body,
        )
        .await;
    }

    let pq = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());

    if state.amp_local.is_some() {
        return Ok(amp_local_fallback_response(
            &method,
            pq,
            format!("no local /api handler matched for path '{path}'"),
        ));
    }

    // Management route — proxy to ampcode.com
    tracing::debug!(
    target: "amp_proxy",
        path = %path,
        "Proxying management route to ampcode.com"
    );
    state
        .amp_management
        .forward(method, pq, headers, body)
        .await
}

/// Handle root-level management routes (`/threads/*`, `/auth/*`, `/docs/*`, etc.).
async fn management_handler(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    let pq = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());

    if is_browser_route(uri.path()) {
        let target = format!("{}{}", state.amp_management.upstream_url, pq);
        tracing::debug!(
            target: "amp_proxy",
            method = %method,
            path = %pq,
            redirect = %target,
            "Redirecting browser route to ampcode.com"
        );
        return Ok(Redirect::temporary(&target).into_response());
    }

    if state.amp_local.is_some() {
        if uri.path() == "/news.rss" {
            return Ok(amp_local_stub_news_rss());
        }
        return Ok(amp_local_fallback_response(
            &method,
            pq,
            "root-level Amp route is not implemented locally",
        ));
    }

    tracing::debug!(
        target: "amp_proxy",
        method = %method,
        path = %pq,
        "Proxying root-level management route to ampcode.com"
    );

    state
        .amp_management
        .forward(method, pq, headers, body)
        .await
}

/// Route a provider request to the appropriate local handler or ampcode.com.
async fn handle_provider(
    state: AppState,
    method: Method,
    provider: &str,
    path: &str,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    // Strip version prefix: v1/, v1beta/, v1beta1/
    let api_path = path
        .strip_prefix("v1/")
        .or_else(|| path.strip_prefix("v1beta/"))
        .or_else(|| path.strip_prefix("v1beta1/"))
        .unwrap_or(path);

    match provider.to_lowercase().as_str() {
        "openai" => handle_openai(state, method, api_path, uri, headers, body).await,
        "anthropic" => handle_anthropic(state, method, api_path, uri, headers, body).await,
        "google" => handle_google(state, method, api_path, uri, headers, body).await,
        _ => {
            let pq = uri
                .path_and_query()
                .map(|pq| pq.as_str())
                .unwrap_or(uri.path());
            if state.amp_local.is_some() {
                return Ok(amp_local_fallback_response(
                    &method,
                    pq,
                    format!("unsupported Amp provider '{provider}'"),
                ));
            }

            // Unsupported provider — forward to ampcode.com
            state
                .amp_management
                .forward(method, pq, headers, body)
                .await
        }
    }
}

/// Handle OpenAI provider requests via Copilot proxy.
async fn handle_openai(
    state: AppState,
    method: Method,
    api_path: &str,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    llm::handle_openai_passthrough(&state, method, api_path, uri.query(), &headers, body).await
}

/// Handle Anthropic/Claude provider requests: convert to OpenAI, forward via Copilot.
async fn handle_anthropic(
    state: AppState,
    method: Method,
    api_path: &str,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    if api_path == "messages" && method == Method::POST {
        let metadata = match analyze_claude_request(&body) {
            Ok(m) => m,
            Err(e) => return Ok(error_from_proxy(e)),
        };

        let use_haiku_rewrite = !metadata.stream
            && metadata.initiator == "user"
            && metadata.model.to_lowercase().contains("haiku");
        if use_haiku_rewrite {
            let mut converted = match convert_claude_request(body) {
                Ok(c) => c,
                Err(e) => return Ok(error_from_proxy(e)),
            };
            converted.body = rewrite_model_in_body(&converted.body, "gpt-5-mini");
            return llm::handle_openai_passthrough(
                &state,
                method,
                "chat/completions",
                uri.query(),
                &headers,
                converted.body,
            )
            .await;
        }
    }

    llm::handle_anthropic_compat(&state, method, api_path, uri.query(), &headers, body, false).await
}

/// Handle Google/Gemini provider requests: convert Gemini native API to OpenAI,
/// forward via Copilot, convert response back to Gemini format.
async fn handle_google(
    state: AppState,
    method: Method,
    api_path: &str,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    // generateContent / streamGenerateContent
    if let Some((model, action)) = parse_gemini_action(api_path) {
        match action {
            "generateContent" | "streamGenerateContent" => {
                let stream = action == "streamGenerateContent";
                llm::handle_gemini_generate_content(
                    &state,
                    method,
                    model,
                    uri.query(),
                    body,
                    stream,
                )
                .await
            }
            "countTokens" => handle_gemini_count_tokens(model, body).await,
            _ => {
                let pq = uri
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or(uri.path());
                if state.amp_local.is_some() {
                    return Ok(amp_local_fallback_response(
                        &method,
                        pq,
                        format!("unsupported Gemini action '{action}'"),
                    ));
                }

                // Unknown action — forward to ampcode.com
                state
                    .amp_management
                    .forward(method, pq, headers, body)
                    .await
            }
        }
    } else {
        let pq = uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or(uri.path());
        if state.amp_local.is_some() {
            return Ok(amp_local_fallback_response(
                &method,
                pq,
                "Gemini route did not match a supported local action",
            ));
        }

        // Models listing, model info, etc. — forward to ampcode.com
        state
            .amp_management
            .forward(method, pq, headers, body)
            .await
    }
}

/// Rewrite the "model" field in a JSON request body.
fn rewrite_model_in_body(body: &Bytes, new_model: &str) -> Bytes {
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(obj) = value.as_object_mut()
    {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(new_model.to_string()),
        );
        if let Ok(bytes) = serde_json::to_vec(&value) {
            return Bytes::from(bytes);
        }
    }
    body.clone()
}
