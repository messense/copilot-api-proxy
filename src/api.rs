//! Top-level `/api/*` router shared by Amp and Droid surfaces.

use crate::error::Error;
use crate::server::AppState;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::routing::any;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/{*path}", any(api_handler))
}

async fn api_handler(
    State(state): State<AppState>,
    method: Method,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    if crate::droid::matches_api_path(&path) {
        return crate::droid::handle_api_request(state, method, &path, &uri, headers, body).await;
    }

    crate::amp::handle_api_request(state, method, &path, &uri, headers, body).await
}
