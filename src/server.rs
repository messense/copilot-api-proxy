//! Axum server: router, handlers, and application state.

use crate::amp::AmpManagementProxy;
use crate::amp::local::LocalAmpState;
use crate::auth::TokenManager;
use crate::droid::DroidManagementProxy;
use crate::droid::local::LocalDroidState;
use crate::error::Error;
use crate::llm;
use crate::proxy::ProxyClient;
use crate::web_backend::SearchProvider;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::Request;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::{any, get};
use std::sync::Arc;
use tower_http::trace::MakeSpan;

#[derive(Clone)]
pub struct AppState {
    pub(crate) proxy: Arc<ProxyClient>,
    pub(crate) amp_management: Arc<AmpManagementProxy>,
    pub(crate) amp_local: Option<Arc<LocalAmpState>>,
    pub(crate) droid_management: Arc<DroidManagementProxy>,
    pub(crate) droid_local: Option<Arc<LocalDroidState>>,
}

impl AppState {
    pub async fn new(
        amp_local: bool,
        droid_local: bool,
        search_provider: SearchProvider,
        search_model: Option<String>,
    ) -> Result<Self, Error> {
        let token = crate::config::load_github_token()?;
        let manager = Arc::new(TokenManager::new(token).await?);
        let proxy = Arc::new(ProxyClient::new(manager)?);
        let amp_management = Arc::new(AmpManagementProxy::new());
        let amp_local_state = if amp_local {
            Some(Arc::new(LocalAmpState::new(
                search_provider.clone(),
                Arc::clone(&proxy),
                search_model.clone(),
            )))
        } else {
            None
        };
        let droid_management = Arc::new(DroidManagementProxy::new());
        let droid_local_state = if droid_local {
            Some(Arc::new(LocalDroidState::new(
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
            droid_management,
            droid_local: droid_local_state,
        })
    }
}

/// Custom span factory that adds empty `initiator` and `upstream` fields to be
/// filled in later when the values are known.
#[derive(Clone)]
struct CopilotMakeSpan;

impl<B> MakeSpan<B> for CopilotMakeSpan {
    fn make_span(&mut self, request: &Request<B>) -> tracing::Span {
        tracing::info_span!(
            "request",
            method = %request.method(),
            uri = %request.uri(),
            initiator = tracing::field::Empty,
            upstream = tracing::field::Empty,
        )
    }
}

/// Record the resolved initiator and upstream path into the current request
/// span so they appear in the TraceLayer's response log line.
pub fn record_upstream(initiator: &str, path: &str) {
    let span = tracing::Span::current();
    span.record("initiator", initiator);
    span.record("upstream", path);
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/usage", get(usage_handler))
        .route("/v1/{*path}", any(proxy_handler))
        .merge(crate::api::routes())
        .merge(crate::amp::root_routes())
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(CopilotMakeSpan))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            10 * 1024 * 1024,
        ))
        .with_state(state)
}

async fn usage_handler(State(state): State<AppState>) -> Result<Response, Error> {
    let resp = state.proxy.fetch_usage().await?;
    crate::proxy::forward_response(resp).await
}

async fn proxy_handler(
    State(state): State<AppState>,
    method: Method,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    if path == "messages" || path == "messages/count_tokens" {
        return llm::handle_anthropic_compat(
            &state,
            method,
            &path,
            uri.query(),
            &headers,
            body,
            true,
        )
        .await;
    }

    llm::handle_openai_passthrough(&state, method, &path, uri.query(), &headers, body).await
}
