//! Amp CLI/IDE integration: provider routing and management proxy.
//!
//! Amp CLI (ampcode.com) sends two kinds of requests:
//! 1. Provider API requests: `/api/provider/{provider}/v1/...` (inference)
//! 2. Management requests: `/api/auth/*`, `/api/threads/*`, `/threads/*`, etc.
//!
//! Provider requests are handled locally through Copilot (OpenAI + Anthropic).
//! Unknown providers and management requests are proxied to ampcode.com.

use crate::claude::{convert_claude_request, convert_openai_response, error_from_proxy};
use crate::error::Error;
use crate::gemini::{
    convert_gemini_request, convert_openai_to_gemini_response, handle_gemini_count_tokens,
    parse_gemini_action,
};
use crate::initiator::{analyze_openai_chat_completions, analyze_openai_responses, RequestAnalysis};
use crate::proxy::forward_response;
use crate::server::AppState;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
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

        let mut req = self.client.request(method, &url);

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

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Create Amp routes to merge into the main router.
pub fn routes() -> Router<AppState> {
    Router::new()
        // All /api/* requests (provider inference + management)
        .route("/api/{*path}", any(amp_api_handler))
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

/// Dispatch `/api/*` requests: provider routes go to Copilot, rest to ampcode.com.
async fn amp_api_handler(
    State(state): State<AppState>,
    method: Method,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    if let Some(rest) = path.strip_prefix("provider/")
        && let Some((provider, provider_path)) = rest.split_once('/') {
            return handle_provider(state, method, provider, provider_path, &uri, headers, body)
                .await;
        }

    // When amp-local mode is enabled, handle management routes locally
    if let Some(ref local_state) = state.amp_local
        && crate::amp_local::should_handle_locally(&path) {
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

    // Management route — proxy to ampcode.com
    let pq = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());
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
            // Unsupported provider (google, etc.) — forward to ampcode.com
            let pq = uri
                .path_and_query()
                .map(|pq| pq.as_str())
                .unwrap_or(uri.path());
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
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
    let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();

    let analysis = analyze_request(api_path, &method, &body);

    let resp = state
        .proxy
        .forward(
            &format!("/{}{}", api_path, query),
            method,
            body,
            content_type,
            analysis.map(|a| a.initiator),
            analysis.map(|a| a.is_vision).unwrap_or(false),
        )
        .await?;
    forward_response(resp).await
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
    let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();

    match api_path {
        "messages/count_tokens" if method == Method::POST => {
            crate::token_counter::handle_count_tokens(body).await
        }
        "messages" if method == Method::POST => {
            // No validate_anthropic_headers for Amp path — auth is handled differently
            let mut converted = match convert_claude_request(body) {
                Ok(c) => c,
                Err(e) => return Ok(error_from_proxy(e)),
            };

            // Use a free model for lightweight non-streaming haiku requests
            // (e.g. titling) to avoid consuming premium Copilot requests.
            if !converted.stream
                && converted.initiator == "user"
                && converted.model.contains("haiku")
            {
                converted.body = rewrite_model_in_body(&converted.body, "gpt-5-mini");
            }

            let resp = match state
                .proxy
                .forward(
                    &format!("/chat/completions{}", query),
                    method,
                    converted.body,
                    Some("application/json"),
                    Some(&converted.initiator),
                    converted.is_vision,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => return Ok(error_from_proxy(e)),
            };
            match convert_openai_response(resp, converted.model, converted.stream).await {
                Ok(r) => Ok(r),
                Err(e) => Ok(error_from_proxy(e)),
            }
        }
        _ => {
            // Other anthropic endpoints (models, etc.) — forward through Copilot
            let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
            let resp = state
                .proxy
                .forward(
                    &format!("/{}{}", api_path, query),
                    method,
                    body,
                    content_type,
                    None,
                    false,
                )
                .await?;
            forward_response(resp).await
        }
    }
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
    let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();

    // generateContent / streamGenerateContent
    if let Some((model, action)) = parse_gemini_action(api_path) {
        match action {
            "generateContent" | "streamGenerateContent" => {
                let stream = action == "streamGenerateContent";
                let converted = match convert_gemini_request(model, body, stream) {
                    Ok(c) => c,
                    Err(e) => return Ok(error_from_proxy(e)),
                };
                let resp = match state
                    .proxy
                    .forward(
                        &format!("/chat/completions{}", query),
                        method,
                        converted.body,
                        Some("application/json"),
                        Some(converted.initiator),
                        converted.is_vision,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return Ok(error_from_proxy(e)),
                };
                match convert_openai_to_gemini_response(resp, converted.model, converted.stream)
                    .await
                {
                    Ok(r) => Ok(r),
                    Err(e) => Ok(error_from_proxy(e)),
                }
            }
            "countTokens" => handle_gemini_count_tokens(model, body).await,
            _ => {
                // Unknown action — forward to ampcode.com
                let pq = uri
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or(uri.path());
                state
                    .amp_management
                    .forward(method, pq, headers, body)
                    .await
            }
        }
    } else {
        // Models listing, model info, etc. — forward to ampcode.com
        let pq = uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or(uri.path());
        state
            .amp_management
            .forward(method, pq, headers, body)
            .await
    }
}

/// Rewrite the "model" field in a JSON request body.
fn rewrite_model_in_body(body: &Bytes, new_model: &str) -> Bytes {
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(obj) = value.as_object_mut() {
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

/// Analyze request body for initiator and vision detection (same logic as server.rs).
fn analyze_request(path: &str, method: &Method, body: &[u8]) -> Option<RequestAnalysis> {
    if *method != Method::POST {
        return None;
    }
    match path {
        "chat/completions" => Some(analyze_openai_chat_completions(body)),
        "responses" => Some(analyze_openai_responses(body)),
        _ => None,
    }
}
