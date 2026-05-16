//! Local Droid control-plane handlers used by `--droid-local`.

use crate::proxy::ProxyClient;
use crate::web_backend::{self, SearchProvider, WebBackend};
use axum::Json;
use axum::body::Bytes;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct LocalDroidState {
    user_id: String,
    org_id: String,
    factory_home: PathBuf,
    web: Box<dyn WebBackend>,
}

impl LocalDroidState {
    pub fn new(
        search_provider: SearchProvider,
        proxy: Arc<ProxyClient>,
        search_model: Option<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client for local droid");
        let web = web_backend::create_backend(&search_provider, http, Some(proxy), search_model);
        Self {
            user_id: std::env::var("DROID_LOCAL_USER_ID").unwrap_or_else(|_| "u_local".to_string()),
            org_id: std::env::var("DROID_LOCAL_ORG_ID").unwrap_or_else(|_| "o_local".to_string()),
            factory_home: resolve_factory_home(),
            web,
        }
    }

    pub fn feature_flags(&self) -> serde_json::Value {
        serde_json::json!({
            "orgId": self.org_id,
            "flags": {},
            "configs": {
                "cli_default_settings": {
                    "enabledPlugins": {},
                    "extraKnownMarketplaces": {}
                },
                "provider_routing": {
                    "version": 1,
                    "defaults": {
                        "anthropic": ["anthropic"],
                        "openai": ["openai"]
                    },
                    "models": {
                        "claude-opus-4-6": ["anthropic"],
                        "claude-opus-4-7": ["anthropic"],
                        "claude-sonnet-4-6": ["anthropic"],
                        "claude-haiku-4-5-20251001": ["anthropic"],
                        "gpt-5.4": ["openai"],
                        "gpt-5.4-mini": ["openai"],
                        "gpt-5.3-codex": ["openai"],
                        "gpt-5.2": ["openai"],
                        "gemini-3.1-pro-preview": ["google"],
                        "gemini-3-flash-preview": ["google"]
                    }
                }
            }
        })
    }

    fn sessions_index_path(&self) -> PathBuf {
        self.factory_home.join("sessions-index.json")
    }
}

pub async fn handle_local_api(
    state: &LocalDroidState,
    method: &Method,
    path: &str,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    match (method, path) {
        // ---- Read endpoints --------------------------------------------
        (&Method::GET, "cli/whoami") => Ok(Json(serde_json::json!({
            "userId": state.user_id,
            "orgId": state.org_id
        }))
        .into_response()),
        (&Method::GET, "cli/org") => Ok(Json(serde_json::json!({
            "workosOrgIds": []
        }))
        .into_response()),
        (&Method::GET, "sessions") => Ok(handle_list_sessions(state)),
        (&Method::GET, "organization/managed-settings") => Ok(Json(serde_json::json!({
            "success": true,
            "settings": {}
        }))
        .into_response()),
        (&Method::GET, "organization/agent-readiness-reports") => {
            // CLI uses `?limit=&startAfter=` and v0.127 switched to
            // URLSearchParams. Empty page is sufficient for local mode.
            Ok(Json(serde_json::json!({
                "reports": [],
                "hasMore": false,
                "nextStartAfter": null
            }))
            .into_response())
        }
        (&Method::GET, "organization/agent-effectiveness/usage")
        | (&Method::POST, "organization/agent-effectiveness/usage") => {
            Ok(Json(serde_json::json!({
                "organizationId": state.org_id,
                "organizationName": "Local Droid",
                "usage": []
            }))
            .into_response())
        }
        (&Method::GET, "organization/members") => Ok(Json(serde_json::json!({
            "members": [],
            "hasMore": false
        }))
        .into_response()),
        (&Method::GET, "feature-flags") => Ok(Json(state.feature_flags()).into_response()),
        (&Method::GET, "hello") => Ok(Json(serde_json::json!({ "ok": true })).into_response()),
        (&Method::GET, "billing/limits") => Ok(Json(serde_json::json!({
            "usesTokenRateLimitsBilling": false
        }))
        .into_response()),
        (&Method::GET, "v0/computers") => {
            Ok(Json(serde_json::json!({ "computers": [] })).into_response())
        }
        (&Method::GET, "v0/automations") => {
            Ok(Json(serde_json::json!({ "automations": [] })).into_response())
        }
        (&Method::GET, "integrations/org/check") => Ok(Json(serde_json::json!({
            "installed": {},
            "available": []
        }))
        .into_response()),
        (&Method::GET, "integrations/scm/repositories") => Ok(Json(serde_json::json!({
            "repositories": [],
            "hasMore": false
        }))
        .into_response()),
        (&Method::GET, "integrations/slack/channels") => {
            Ok(Json(serde_json::json!({ "channels": [] })).into_response())
        }
        (&Method::GET, "integrations/slack/listening-channels") => {
            Ok(Json(serde_json::json!({ "listeningChannels": [] })).into_response())
        }

        // ---- Session writes --------------------------------------------
        (&Method::POST, "sessions/create") => Ok(handle_create_session(body)),
        _ if *method == Method::POST && path.ends_with("/update-title") => {
            Ok(handle_update_title(state, path, body))
        }
        _ if *method == Method::POST && is_session_write(path) => Ok(ok_response()),
        (&Method::POST, "integrations/slack/listening-channels/enable")
        | (&Method::PATCH, "integrations/slack/listening-channels/settings") => {
            Ok(Json(serde_json::json!({ "listeningChannels": [] })).into_response())
        }
        (&Method::POST, "organization/subscription/set-overage-preference") => Ok(ok_response()),
        (&Method::POST, "automations/sync") => Ok(Json(serde_json::json!({
            "synced": 0,
            "collisions": []
        }))
        .into_response()),
        _ if *method == Method::POST
            && path.starts_with("automations/")
            && path.ends_with("/visual") =>
        {
            Ok(ok_response())
        }
        (&Method::POST, "bug-reports") => Ok(Json(serde_json::json!({
            "bugReportId": format!("local-{}", uuid::Uuid::new_v4())
        }))
        .into_response()),
        (&Method::POST, "tools/web-search") => handle_web_search(state, body).await,
        (&Method::POST, "tools/get-url-contents") => handle_get_url_contents(state, body).await,
        (&Method::POST, "tools/slack/post-message") => Ok(Json(serde_json::json!({
            "isError": true,
            "errorType": "externalAPIError",
            "llmError": "Slack integration is unavailable in --droid-local",
            "userError": "Slack integration is unavailable in local mode"
        }))
        .into_response()),

        // ---- Telemetry / fire-and-forget POSTs -------------------------
        // Bare paths used by current droid CLI (v0.122.0+) plus historical
        // `/api/telemetry/...` aliases kept for backward compatibility.
        (&Method::POST, "ingest")
        | (&Method::POST, "otlp/traces/ingest")
        | (&Method::POST, "daemon/heartbeat")
        | (&Method::POST, "organization/agent-readiness-reports")
        | (&Method::POST, "llm/custom/usage")
        | (&Method::POST, "llm/failed-requests")
        | (&Method::POST, "telemetry/cli-ingest")
        | (&Method::POST, "telemetry/otlp/traces/ingest") => Ok(ok_response()),

        _ => Ok(not_implemented(method, path)),
    }
}

fn handle_create_session(body: &Bytes) -> Response {
    let id = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("id")
                .and_then(|id| id.as_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    Json(serde_json::json!({
        "id": id,
        "sessionId": id
    }))
    .into_response()
}

fn handle_list_sessions(state: &LocalDroidState) -> Response {
    match read_sessions_index(&state.sessions_index_path()) {
        Ok(index) => Json(index).into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": {
                    "message": message,
                    "type": "droid_local_session_index_error",
                    "path": "/api/sessions",
                    "method": "GET"
                }
            })),
        )
            .into_response(),
    }
}

async fn handle_web_search(
    state: &LocalDroidState,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let value = parse_json_body(body);
    let query = value
        .get("query")
        .or_else(|| value.get("q"))
        .and_then(|query| query.as_str())
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        return Ok(invalid_local_request(
            "/api/tools/web-search",
            "missing query in web-search body",
        ));
    }
    let max_results = value
        .get("maxResults")
        .or_else(|| value.get("limit"))
        .and_then(|max_results| max_results.as_u64())
        .unwrap_or(5) as usize;

    match state.web.search(vec![query.to_string()], max_results).await {
        Ok(results) => Ok(Json(serde_json::json!({
            "results": results.into_iter().map(|result| serde_json::json!({
                "title": result.title,
                "url": result.url,
                "summary": result.content
            })).collect::<Vec<_>>()
        }))
        .into_response()),
        Err(message) => Ok(Json(serde_json::json!({
            "results": [],
            "error": { "message": message }
        }))
        .into_response()),
    }
}

async fn handle_get_url_contents(
    state: &LocalDroidState,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let value = parse_json_body(body);
    let url = value
        .get("url")
        .and_then(|url| url.as_str())
        .unwrap_or_default()
        .trim();
    if url.is_empty() {
        return Ok(invalid_local_request(
            "/api/tools/get-url-contents",
            "missing url in get-url-contents body",
        ));
    }

    match state.web.extract_page(url.to_string()).await {
        Ok(page) if !page.full_content.trim().is_empty() => Ok(Json(serde_json::json!({
            "data": {
                "markdown": page.full_content,
                "title": null,
                "metadata": {
                    "url": url,
                    "statusCode": 200,
                    "error": null,
                    "title": null
                },
                "linkedUrls": []
            }
        }))
        .into_response()),
        Ok(_) => Ok(Json(serde_json::json!({
            "error": {
                "message": "URL content API returned no content in --droid-local"
            }
        }))
        .into_response()),
        Err(message) => Ok(Json(serde_json::json!({
            "error": { "message": message }
        }))
        .into_response()),
    }
}

fn parse_json_body(body: &Bytes) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

fn handle_update_title(state: &LocalDroidState, path: &str, body: &Bytes) -> Response {
    let Some(session_id) = path
        .strip_prefix("sessions/")
        .and_then(|rest| rest.strip_suffix("/update-title"))
    else {
        return invalid_local_request(
            "/api/sessions/{id}/update-title",
            "invalid session update-title path",
        );
    };

    let Some(title) = extract_title(body) else {
        return invalid_local_request(
            "/api/sessions/{id}/update-title",
            "missing title in update-title body",
        );
    };

    match apply_title_update(state, session_id, &title) {
        Ok(()) => ok_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": {
                    "message": message,
                    "type": "droid_local_update_title_error",
                    "path": format!("/api/sessions/{session_id}/update-title"),
                    "method": "POST"
                }
            })),
        )
            .into_response(),
    }
}

fn is_session_write(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("sessions/") else {
        return false;
    };
    // Strip the `{id}/` segment.
    let Some((_id, tail)) = rest.split_once('/') else {
        return false;
    };
    matches!(
        tail,
        "update-settings"
            | "message/create"
            | "update-title"
            | "droid-status"
            | "archive"
            | "unarchive"
            | "privacy"
            | "git-ai/checkpoints"
            | "git-ai/notes"
            | "git-ai/pull-requests"
    )
}

fn resolve_factory_home() -> PathBuf {
    if let Some(path) = std::env::var_os("FACTORY_HOME_OVERRIDE") {
        return PathBuf::from(path);
    }

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".factory")
}

fn read_sessions_index(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| {
            format!(
                "failed to parse local session index at {}: {err}",
                path.display()
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::json!({ "version": 1, "entries": [] }))
        }
        Err(err) => Err(format!(
            "failed to read local session index at {}: {err}",
            path.display()
        )),
    }
}

fn write_sessions_index(path: &Path, index: &serde_json::Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(index).map_err(|err| {
        format!(
            "failed to serialize local session index at {}: {err}",
            path.display()
        )
    })?;
    std::fs::write(path, bytes).map_err(|err| {
        format!(
            "failed to write local session index at {}: {err}",
            path.display()
        )
    })
}

fn extract_title(body: &Bytes) -> Option<String> {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    value
        .get("title")
        .and_then(|title| title.as_str())
        .or_else(|| value.get("sessionTitle").and_then(|title| title.as_str()))
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(ToString::to_string)
}

fn apply_title_update(
    state: &LocalDroidState,
    session_id: &str,
    title: &str,
) -> Result<(), String> {
    let index_path = state.sessions_index_path();
    let mut index = read_sessions_index(&index_path)?;
    update_index_title(&mut index, session_id, title)?;
    write_sessions_index(&index_path, &index)
}

fn update_index_title(
    index: &mut serde_json::Value,
    session_id: &str,
    title: &str,
) -> Result<(), String> {
    let Some(entries) = index
        .get_mut("entries")
        .and_then(|entries| entries.as_array_mut())
    else {
        return Err("local session index is missing entries array".to_string());
    };

    let Some(entry) = entries.iter_mut().find(|entry| {
        entry
            .get("sessionId")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value == session_id)
    }) else {
        return Err(format!(
            "session {session_id} not found in local session index"
        ));
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| format!("failed to compute current timestamp: {err}"))?
        .as_millis() as u64;

    entry["title"] = serde_json::Value::String(title.to_string());
    entry["mtime"] = serde_json::Value::Number(now_ms.into());
    Ok(())
}

fn ok_response() -> Response {
    Json(serde_json::json!({ "ok": true })).into_response()
}

fn invalid_local_request(path: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "path": path,
                "method": "POST"
            }
        })),
    )
        .into_response()
}

fn not_implemented(method: &Method, path: &str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": {
                "message": format!(
                    "--droid-local blocked unsupported route {} /api/{}",
                    method, path
                ),
                "type": "droid_local_unimplemented",
                "path": format!("/api/{path}"),
                "method": method.as_str()
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        LocalDroidState, apply_title_update, extract_title, handle_local_api, is_session_write,
        read_sessions_index,
    };
    use axum::body::{Bytes, to_bytes};
    use axum::http::{Method, StatusCode};
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "copilot-api-proxy-droid-local-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    fn test_state() -> LocalDroidState {
        LocalDroidState {
            user_id: "u".to_string(),
            org_id: "o".to_string(),
            factory_home: temp_path("factory-home"),
            web: crate::web_backend::create_backend(
                &crate::web_backend::SearchProvider::None,
                reqwest::Client::new(),
                None,
                None,
            ),
        }
    }

    async fn local_json(method: Method, path: &str) -> serde_json::Value {
        local_json_with_body(method, path, Bytes::new()).await
    }

    async fn local_json_with_body(method: Method, path: &str, body: Bytes) -> serde_json::Value {
        let state = test_state();
        let response = handle_local_api(&state, &method, path, &body)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn recognizes_supported_session_write_paths() {
        assert!(is_session_write("sessions/abc/update-settings"));
        assert!(is_session_write("sessions/abc/message/create"));
        assert!(is_session_write("sessions/abc/update-title"));
        assert!(is_session_write("sessions/abc/droid-status"));
        assert!(is_session_write("sessions/abc/archive"));
        assert!(is_session_write("sessions/abc/unarchive"));
        assert!(is_session_write("sessions/abc/privacy"));
        assert!(is_session_write("sessions/abc/git-ai/checkpoints"));
        assert!(is_session_write("sessions/abc/git-ai/notes"));
        assert!(is_session_write("sessions/abc/git-ai/pull-requests"));
    }

    #[test]
    fn rejects_non_session_write_paths() {
        assert!(!is_session_write("sessions"));
        assert!(!is_session_write("sessions/create"));
        assert!(!is_session_write("sessions/abc"));
        assert!(!is_session_write("sessions/abc/message"));
        assert!(!is_session_write("sessions/abc/unknown"));
        assert!(!is_session_write("telemetry/cli-ingest"));
        assert!(!is_session_write("ingest"));
    }

    #[tokio::test]
    async fn returns_empty_offline_shapes_for_current_droid_routes() {
        assert_eq!(
            local_json(Method::GET, "cli/org").await,
            serde_json::json!({ "workosOrgIds": [] })
        );
        assert_eq!(
            local_json(Method::GET, "billing/limits").await,
            serde_json::json!({ "usesTokenRateLimitsBilling": false })
        );
        assert_eq!(
            local_json(Method::GET, "organization/agent-readiness-reports").await,
            serde_json::json!({
                "reports": [],
                "hasMore": false,
                "nextStartAfter": null
            })
        );
        assert_eq!(
            local_json(Method::GET, "organization/agent-effectiveness/usage").await,
            serde_json::json!({
                "organizationId": "o",
                "organizationName": "Local Droid",
                "usage": []
            })
        );
        assert_eq!(
            local_json(Method::POST, "organization/agent-effectiveness/usage").await,
            serde_json::json!({
                "organizationId": "o",
                "organizationName": "Local Droid",
                "usage": []
            })
        );
        assert_eq!(
            local_json(Method::GET, "organization/members").await,
            serde_json::json!({ "members": [], "hasMore": false })
        );
        assert_eq!(
            local_json(Method::GET, "v0/computers").await,
            serde_json::json!({ "computers": [] })
        );
        assert_eq!(
            local_json(Method::GET, "v0/automations").await,
            serde_json::json!({ "automations": [] })
        );
        assert_eq!(
            local_json(Method::GET, "integrations/slack/channels").await,
            serde_json::json!({ "channels": [] })
        );
        assert_eq!(
            local_json(Method::GET, "integrations/scm/repositories").await,
            serde_json::json!({ "repositories": [], "hasMore": false })
        );
        assert_eq!(
            local_json(Method::GET, "integrations/slack/listening-channels").await,
            serde_json::json!({ "listeningChannels": [] })
        );
        assert_eq!(
            local_json(Method::POST, "integrations/slack/listening-channels/enable").await,
            serde_json::json!({ "listeningChannels": [] })
        );
        assert_eq!(
            local_json(
                Method::PATCH,
                "integrations/slack/listening-channels/settings"
            )
            .await,
            serde_json::json!({ "listeningChannels": [] })
        );
        assert_eq!(
            local_json(Method::POST, "automations/sync").await,
            serde_json::json!({ "synced": 0, "collisions": [] })
        );
        assert_eq!(
            local_json_with_body(
                Method::POST,
                "tools/web-search",
                Bytes::from_static(br#"{"query":"rust"}"#)
            )
            .await,
            serde_json::json!({ "results": [] })
        );

        let fetch_url = local_json_with_body(
            Method::POST,
            "tools/get-url-contents",
            Bytes::from_static(br#"{"url":"https://example.com"}"#),
        )
        .await;
        assert!(fetch_url.get("error").is_some());

        let slack_post = local_json(Method::POST, "tools/slack/post-message").await;
        assert_eq!(slack_post["isError"], true);

        assert_eq!(
            local_json(Method::POST, "sessions/abc/git-ai/pull-requests").await,
            serde_json::json!({ "ok": true })
        );
    }

    #[test]
    fn reads_existing_sessions_index_verbatim() {
        let path = temp_path("index");
        let expected = serde_json::json!({
            "version": 1,
            "entries": [
                {
                    "sessionId": "session-1",
                    "mtime": 1,
                    "settingsMtime": 2,
                    "title": "Example",
                    "cwd": "/tmp/example",
                    "messagesCount": 3,
                    "archivedAt": 4
                }
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&expected).unwrap()).unwrap();

        let actual = read_sessions_index(&path).unwrap();
        assert_eq!(actual, expected);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_sessions_index_returns_empty_index() {
        let path = temp_path("missing");
        let actual = read_sessions_index(&path).unwrap();
        assert_eq!(actual, serde_json::json!({ "version": 1, "entries": [] }));
    }

    #[test]
    fn invalid_sessions_index_returns_error() {
        let path = temp_path("invalid");
        std::fs::write(&path, b"not json").unwrap();

        let err = read_sessions_index(&path).unwrap_err();
        assert!(err.contains("failed to parse local session index"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extracts_title_from_update_title_body() {
        let title = extract_title(&Bytes::from_static(br#"{"title":"Renamed"}"#));
        assert_eq!(title.as_deref(), Some("Renamed"));

        let title = extract_title(&Bytes::from_static(br#"{"sessionTitle":"Renamed"}"#));
        assert_eq!(title.as_deref(), Some("Renamed"));
    }

    #[test]
    fn apply_title_update_updates_index_only() {
        let factory_home = temp_path("factory-home");
        std::fs::create_dir_all(factory_home.join("sessions")).unwrap();

        std::fs::write(
            factory_home.join("sessions-index.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "entries": [
                    {
                        "sessionId": "session-1",
                        "mtime": 1,
                        "settingsMtime": 2,
                        "title": "Old Title",
                        "cwd": "/tmp/example",
                        "messagesCount": 3
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let state = super::LocalDroidState {
            user_id: "u".to_string(),
            org_id: "o".to_string(),
            factory_home: factory_home.clone(),
            web: crate::web_backend::create_backend(
                &crate::web_backend::SearchProvider::None,
                reqwest::Client::new(),
                None,
                None,
            ),
        };
        apply_title_update(&state, "session-1", "Renamed").unwrap();

        let index = read_sessions_index(&factory_home.join("sessions-index.json")).unwrap();
        assert_eq!(index["entries"][0]["title"], "Renamed");

        let _ = std::fs::remove_dir_all(factory_home);
    }
}
