//! Axum server: router, handlers, and application state.

use crate::amp::AmpManagementProxy;
use crate::amp_local::LocalAmpState;
use crate::auth::TokenManager;
use crate::web_backend::SearchProvider;
use crate::claude::{
    convert_claude_request, convert_openai_response, error_from_proxy, validate_anthropic_headers,
};
use crate::error::Error;
use crate::initiator::{
    RequestAnalysis, analyze_openai_chat_completions, analyze_openai_responses,
};
use crate::proxy::{ProxyClient, forward_response};
use crate::token_counter::handle_count_tokens;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::any;
use std::sync::Arc;

/// Analyze request body for initiator and vision detection.
/// Returns analysis for chat/responses endpoints, None for others.
fn analyze_request(path: &str, method: &Method, body: &[u8]) -> Option<RequestAnalysis> {
    if method != Method::POST {
        return None;
    }
    match path {
        "chat/completions" => Some(analyze_openai_chat_completions(body)),
        "responses" => Some(analyze_openai_responses(body)),
        _ => None,
    }
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) proxy: Arc<ProxyClient>,
    pub(crate) amp_management: Arc<AmpManagementProxy>,
    pub(crate) amp_local: Option<Arc<LocalAmpState>>,
}

impl AppState {
    pub async fn new(
        amp_local: bool,
        search_provider: SearchProvider,
        search_model: Option<String>,
    ) -> Result<Self, Error> {
        let token = crate::config::load_github_token()?;
        let manager = Arc::new(TokenManager::new(token).await?);
        let proxy = Arc::new(ProxyClient::new(manager)?);
        let amp_management = Arc::new(AmpManagementProxy::new());
        let amp_local_state = if amp_local {
            Some(Arc::new(LocalAmpState::new(
                search_provider,
                Arc::clone(&proxy),
                search_model,
            )))
        } else {
            None
        };
        Ok(Self {
            proxy,
            amp_management,
            amp_local: amp_local_state,
        })
    }
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/{*path}", any(proxy_handler))
        .merge(crate::amp::routes())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            10 * 1024 * 1024,
        ))
        .with_state(state)
}

async fn proxy_handler(
    State(state): State<AppState>,
    method: Method,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
    let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();

    // Special case: Anthropic token counting
    if path == "messages/count_tokens" {
        if method != Method::POST {
            return Ok(error_from_proxy(Error::InvalidRequest(
                "Only POST is supported for /v1/messages/count_tokens".to_string(),
            )));
        }
        return handle_count_tokens(body).await;
    }

    if path == "messages" {
        if method != Method::POST {
            return Ok(error_from_proxy(Error::InvalidRequest(
                "Only POST is supported for /v1/messages".to_string(),
            )));
        }

        if let Some(resp) = validate_anthropic_headers(&headers) {
            return Ok(resp);
        }

        let converted = match convert_claude_request(body) {
            Ok(converted) => converted,
            Err(err) => return Ok(error_from_proxy(err)),
        };
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
            Ok(resp) => resp,
            Err(err) => return Ok(error_from_proxy(err)),
        };
        let response = match convert_openai_response(resp, converted.model, converted.stream).await
        {
            Ok(response) => response,
            Err(err) => return Ok(error_from_proxy(err)),
        };
        return Ok(response);
    }

    // Analyze request for initiator and vision detection
    let analysis = analyze_request(&path, &method, &body);

    let resp = state
        .proxy
        .forward(
            &format!("/{}{}", path, query),
            method,
            body,
            content_type,
            analysis.map(|a| a.initiator),
            analysis.map(|a| a.is_vision).unwrap_or(false),
        )
        .await?;
    forward_response(resp).await
}
