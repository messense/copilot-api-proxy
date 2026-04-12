//! Axum server: router, handlers, and application state.

use crate::amp::AmpManagementProxy;
use crate::amp_local::LocalAmpState;
use crate::auth::TokenManager;
use crate::droid::DroidManagementProxy;
use crate::droid_local::LocalDroidState;
use crate::error::Error;
use crate::llm;
use crate::proxy::ProxyClient;
use crate::web_backend::SearchProvider;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::any;
use std::sync::Arc;

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
                search_provider,
                Arc::clone(&proxy),
                search_model,
            )))
        } else {
            None
        };
        let droid_management = Arc::new(DroidManagementProxy::new());
        let droid_local_state = if droid_local {
            Some(Arc::new(LocalDroidState::new()))
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

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/{*path}", any(proxy_handler))
        .merge(crate::api::routes())
        .merge(crate::amp::root_routes())
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
