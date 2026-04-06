//! Local Amp API handlers that replace ampcode.com for offline usage.
//!
//! This module manages threads in the local Amp data directory
//! (`~/.local/share/amp/threads/`) and serves them through the same
//! API surface that the Amp CLI expects from ampcode.com.
//!
//! Supported endpoints:
//! - `GET  /api/threads/find?q=&limit=&offset=` — full-text search across local threads
//! - `GET  /api/threads/{id}.md?truncate_tool_results=1` — render thread as markdown
//! - `POST /api/internal?uploadThread` — persist full thread JSON to local disk
//! - `POST /api/internal?setThreadMeta` — merge meta fields on an existing thread
//! - `POST /api/internal?deleteThread` — remove a thread file from disk
//! - `POST /api/internal?{method}` — getUserInfo, shareThread, getThreadLabels, setThreadLabels, …
//! - `POST /api/telemetry` — silently accepted (no-op)
//! - `POST /api/durable-thread-workers/{id}` — stub response
//!
//! The Amp CLI gzip-compresses request bodies larger than ~25 KB.
//! Thread write handlers transparently decompress when `Content-Encoding: gzip` is set.

use crate::proxy::ProxyClient;
use crate::web_backend::{self, SearchProvider, WebBackend};
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Thread data structures (matching local ~/.local/share/amp/threads/*.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ThreadFile {
    pub v: Option<u64>,
    pub id: String,
    pub created: Option<u64>,
    pub messages: Option<Vec<ThreadMessage>>,
    #[serde(rename = "agentMode")]
    pub agent_mode: Option<String>,
    #[serde(rename = "nextMessageId")]
    pub next_message_id: Option<u64>,
    pub title: Option<String>,
    pub env: Option<serde_json::Value>,
    pub meta: Option<serde_json::Value>,
    #[serde(rename = "~debug")]
    pub debug: Option<serde_json::Value>,
    #[serde(rename = "activatedSkills")]
    pub activated_skills: Option<Vec<serde_json::Value>>,
    pub relationships: Option<Vec<serde_json::Value>>,
    pub archived: Option<bool>,
    #[serde(rename = "originThreadID")]
    pub origin_thread_id: Option<String>,
    #[serde(rename = "mainThreadID")]
    pub main_thread_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ThreadMessage {
    pub role: Option<String>,
    #[serde(rename = "messageId")]
    pub message_id: Option<u64>,
    pub content: Option<Vec<ContentBlock>>,
    #[serde(rename = "userState")]
    pub user_state: Option<serde_json::Value>,
    #[serde(rename = "agentMode")]
    pub agent_mode: Option<String>,
    pub meta: Option<serde_json::Value>,
    pub state: Option<serde_json::Value>,
    pub usage: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    pub text: Option<String>,
    pub name: Option<String>,
    pub input: Option<serde_json::Value>,
    pub content: Option<serde_json::Value>,
    pub tool_use_id: Option<String>,
}

// ---------------------------------------------------------------------------
// API response structures (matching what amp CLI expects)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ThreadSearchResult {
    threads: Vec<ThreadSearchEntry>,
    #[serde(rename = "hasMore")]
    has_more: bool,
}

#[derive(Serialize)]
struct ThreadSearchEntry {
    id: String,
    title: Option<String>,
    #[serde(rename = "creatorUserID")]
    creator_user_id: Option<String>,
    created: u64,
    #[serde(rename = "updatedAt")]
    updated_at: u64,
    #[serde(rename = "messageCount")]
    message_count: u64,
    #[serde(rename = "matchedSearchText")]
    matched_search_text: Option<String>,
}

// ---------------------------------------------------------------------------
// Thread index: in-memory index built from local thread files
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct IndexedThread {
    id: String,
    title: Option<String>,
    created: u64,
    updated_at: u64,
    message_count: u64,
    agent_mode: Option<String>,
    v: u64,
    env: Option<serde_json::Value>,
    relationships: Vec<serde_json::Value>,
    uses_dtw: bool,
    archived: bool,
    /// Concatenated searchable text (title + first user messages)
    search_text: String,
}

pub struct LocalAmpState {
    threads_dir: PathBuf,
    index: RwLock<Vec<IndexedThread>>,
    /// Timestamp of last index rebuild
    last_indexed: RwLock<std::time::Instant>,
    /// Pluggable web search / page-extract backend
    web: Box<dyn WebBackend>,
}

impl LocalAmpState {
    pub fn new(
        search_provider: SearchProvider,
        proxy: Arc<ProxyClient>,
        search_model: Option<String>,
    ) -> Self {
        let threads_dir = amp_threads_dir();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client for local amp");
        let web = web_backend::create_backend(&search_provider, http, Some(proxy), search_model);
        tracing::info!("Search provider: {}", search_provider);
        Self {
            threads_dir,
            index: RwLock::new(Vec::new()),
            last_indexed: RwLock::new(
                std::time::Instant::now() - std::time::Duration::from_secs(3600),
            ),
            web,
        }
    }

    /// Rebuild the index if it's stale (older than 5 seconds).
    async fn ensure_index(&self) {
        let stale = {
            let last = self.last_indexed.read().await;
            last.elapsed() > std::time::Duration::from_secs(5)
        };
        if stale {
            self.rebuild_index().await;
        }
    }

    async fn rebuild_index(&self) {
        let dir = &self.threads_dir;
        if !dir.exists() {
            tracing::debug!("Amp threads directory does not exist: {}", dir.display());
            return;
        }
        let mut entries = Vec::new();
        let mut read_dir = match tokio::fs::read_dir(dir).await {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!("Failed to read amp threads dir: {}", e);
                return;
            }
        };
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match read_thread_index_entry(&path).await {
                Some(idx) => entries.push(idx),
                None => continue,
            }
        }
        // Sort by updated_at descending (newest first)
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let count = entries.len();
        *self.index.write().await = entries;
        *self.last_indexed.write().await = std::time::Instant::now();
        tracing::debug!("Rebuilt local amp thread index: {} threads", count);
    }
}

async fn read_thread_index_entry(path: &Path) -> Option<IndexedThread> {
    let data = tokio::fs::read(path).await.ok()?;
    let thread: ThreadFile = serde_json::from_slice(&data).ok()?;

    let messages = thread.messages.as_ref();
    let message_count = messages.map(|m| m.len() as u64).unwrap_or(0);

    // Build searchable text from title and first few user messages
    let mut search_parts: Vec<String> = Vec::new();
    if let Some(ref t) = thread.title {
        search_parts.push(t.clone());
    }
    if let Some(msgs) = messages {
        for msg in msgs.iter().take(20) {
            if (msg.role.as_deref() == Some("user") || msg.role.as_deref() == Some("assistant"))
                && let Some(ref content) = msg.content {
                    for block in content {
                        if block.content_type.as_deref() == Some("text")
                            && let Some(ref text) = block.text {
                                search_parts.push(text.clone());
                            }
                    }
                }
        }
    }

    // Compute updated_at: use the latest message sentAt, or created
    let created = thread.created.unwrap_or(0);
    let mut updated_at = created;
    if let Some(msgs) = messages {
        for msg in msgs.iter().rev().take(5) {
            if let Some(ref meta) = msg.meta
                && let Some(sent) = meta.get("sentAt").and_then(|v| v.as_u64())
                    && sent > updated_at {
                        updated_at = sent;
                    }
        }
    }

    // Detect usesDtw from meta
    let uses_dtw = thread
        .meta
        .as_ref()
        .and_then(|m| m.get("usesDtw"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Some(IndexedThread {
        id: thread.id.clone(),
        title: thread.title.clone(),
        created,
        updated_at,
        message_count,
        agent_mode: thread.agent_mode.clone(),
        v: thread.v.unwrap_or(0),
        env: thread.env.clone(),
        relationships: thread.relationships.clone().unwrap_or_default(),
        uses_dtw,
        archived: thread.archived.unwrap_or(false),
        search_text: search_parts.join(" ").to_lowercase(),
    })
}

fn amp_threads_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AMP_THREADS_DIR") {
        return PathBuf::from(dir);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("share").join("amp").join("threads")
}

fn amp_data_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("share").join("amp")
}

// ---------------------------------------------------------------------------
// Public API: determine if a path should be handled locally
// ---------------------------------------------------------------------------

/// Returns true if this management API path should be handled locally
/// instead of being proxied to ampcode.com.
pub fn should_handle_locally(path: &str) -> bool {
    // Thread search & markdown
    if path.starts_with("threads/find") || path.starts_with("threads/") {
        return true;
    }
    // Internal RPC
    if path.starts_with("internal") {
        return true;
    }
    // Telemetry (no-op)
    if path == "telemetry" {
        return true;
    }
    // Durable thread workers
    if path.starts_with("durable-thread-workers") {
        return true;
    }
    // Users
    if path.starts_with("users/") {
        return true;
    }
    // Attachments (stub)
    if path == "attachments" {
        return true;
    }
    false
}

/// Handle a local Amp API request.
pub async fn handle_local_api(
    state: &Arc<LocalAmpState>,
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    // /api/threads/find?q=&limit=&offset=
    if path.starts_with("threads/find") {
        return handle_thread_search(state, query).await;
    }

    // /api/threads/{id}.md
    if path.starts_with("threads/") && path.ends_with(".md") {
        let id = path
            .strip_prefix("threads/")
            .and_then(|p| p.strip_suffix(".md"))
            .unwrap_or("");
        return handle_thread_markdown(state, id, query).await;
    }

    // /api/internal?method
    if path.starts_with("internal") {
        return handle_internal_rpc(state, query, headers, body).await;
    }

    // /api/telemetry — accept silently
    if path == "telemetry" && *method == Method::POST {
        return Ok(StatusCode::OK.into_response());
    }

    // /api/durable-thread-workers/{id}
    if path.starts_with("durable-thread-workers") {
        return handle_durable_thread_workers(path).await;
    }

    // /api/users/{id}
    if path.starts_with("users/") {
        return handle_user_info().await;
    }

    // /api/attachments
    if path == "attachments" {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({"attachments": []})),
        )
            .into_response());
    }

    // Fallback
    Ok((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "Not found"})),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Handler: /api/threads/find
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ThreadSearchParams {
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn handle_thread_search(
    state: &Arc<LocalAmpState>,
    query_str: Option<&str>,
) -> Result<Response, crate::Error> {
    state.ensure_index().await;

    let params: ThreadSearchParams = query_str
        .map(|q| serde_urlencoded::from_str(q).unwrap_or(ThreadSearchParams {
            q: None,
            limit: None,
            offset: None,
        }))
        .unwrap_or(ThreadSearchParams {
            q: None,
            limit: None,
            offset: None,
        });

    let query_lower = params
        .q
        .as_deref()
        .unwrap_or("")
        .to_lowercase();
    let limit = params.limit.unwrap_or(20).min(100);
    let offset = params.offset.unwrap_or(0);

    let index = state.index.read().await;

    // Parse search query for special filters
    let (text_query, filters) = parse_search_query(&query_lower);

    let matching: Vec<&IndexedThread> = index
        .iter()
        .filter(|t| {
            // Apply filters
            if let Some(ref author_filter) = filters.author {
                // We don't have author info locally, skip author filtering
                let _ = author_filter;
            }
            if let Some(ref file_filter) = filters.file {
                // Check if searchable text mentions the file
                if !t.search_text.contains(&file_filter.to_lowercase()) {
                    return false;
                }
            }

            // Text search
            if text_query.is_empty() {
                return true;
            }

            // Split query into words and require all of them to match
            let words: Vec<&str> = text_query.split_whitespace().collect();
            words.iter().all(|w| t.search_text.contains(w))
        })
        .collect();

    let total = matching.len();
    let has_more = offset + limit < total;
    let page: Vec<ThreadSearchEntry> = matching
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|t| {
            // Find matched text snippet
            let matched = if !text_query.is_empty() {
                find_match_snippet(&t.search_text, &text_query)
            } else {
                None
            };
            ThreadSearchEntry {
                id: t.id.clone(),
                title: t.title.clone(),
                creator_user_id: Some("local-user".into()),
                created: t.created,
                updated_at: t.updated_at,
                message_count: t.message_count,
                matched_search_text: matched,
            }
        })
        .collect();

    Ok(Json(ThreadSearchResult {
        threads: page,
        has_more,
    })
    .into_response())
}

struct SearchFilters {
    author: Option<String>,
    file: Option<String>,
}

fn parse_search_query(query: &str) -> (String, SearchFilters) {
    let mut text_parts = Vec::new();
    let mut filters = SearchFilters {
        author: None,
        file: None,
    };

    for token in query.split_whitespace() {
        if let Some(author) = token.strip_prefix("author:") {
            filters.author = Some(author.to_string());
        } else if let Some(file) = token.strip_prefix("file:") {
            filters.file = Some(file.to_string());
        } else {
            text_parts.push(token);
        }
    }

    (text_parts.join(" "), filters)
}

fn find_match_snippet(text: &str, query: &str) -> Option<String> {
    let first_word = query.split_whitespace().next()?;
    let pos = text.find(first_word)?;
    // Snap to char boundaries
    let start = text.floor_char_boundary(pos.saturating_sub(40));
    let end = text.ceil_char_boundary((pos + first_word.len() + 80).min(text.len()));
    Some(text[start..end].to_string())
}

// ---------------------------------------------------------------------------
// Handler: /api/threads/{id}.md
// ---------------------------------------------------------------------------

async fn handle_thread_markdown(
    state: &Arc<LocalAmpState>,
    id: &str,
    query_str: Option<&str>,
) -> Result<Response, crate::Error> {
    let truncate_tool_results = query_str
        .map(|q| q.contains("truncate_tool_results=1"))
        .unwrap_or(false);

    let path = state.threads_dir.join(format!("{}.json", id));
    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(_) => {
            return Ok((
                StatusCode::NOT_FOUND,
                format!("Thread {} not found", id),
            )
                .into_response());
        }
    };

    let thread: ThreadFile = match serde_json::from_slice(&data) {
        Ok(t) => t,
        Err(e) => {
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to parse thread: {}", e),
            )
                .into_response());
        }
    };

    let markdown = thread_to_markdown(&thread, truncate_tool_results);

    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        markdown,
    )
        .into_response())
}

fn thread_to_markdown(thread: &ThreadFile, truncate_tool_results: bool) -> String {
    let mut md = String::new();

    // Header
    let title = thread.title.as_deref().unwrap_or("Untitled Thread");
    md.push_str(&format!("# {}\n\n", title));
    md.push_str(&format!("Thread ID: {}\n", thread.id));
    if let Some(mode) = &thread.agent_mode {
        md.push_str(&format!("Mode: {}\n", mode));
    }
    if let Some(created) = thread.created {
        let dt = chrono_format_millis(created);
        md.push_str(&format!("Created: {}\n", dt));
    }

    // Environment info
    if let Some(ref env) = thread.env
        && let Some(initial) = env.get("initial")
            && let Some(trees) = initial.get("trees").and_then(|t| t.as_array())
                && !trees.is_empty() {
                    md.push_str("\n## Workspace\n\n");
                    for tree in trees {
                        if let Some(name) = tree.get("displayName").and_then(|n| n.as_str()) {
                            md.push_str(&format!("- {}", name));
                            if let Some(repo) = tree.get("repository")
                                && let Some(url) = repo.get("url").and_then(|u| u.as_str()) {
                                    md.push_str(&format!(" ({})", url));
                                }
                            md.push('\n');
                        }
                    }
                }

    // Skills
    if let Some(ref skills) = thread.activated_skills
        && !skills.is_empty() {
            let names: Vec<String> = skills
                .iter()
                .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect();
            if !names.is_empty() {
                md.push_str(&format!("\nSkills used: {}\n", names.join(", ")));
            }
        }

    md.push_str("\n---\n\n");

    // Messages
    if let Some(ref messages) = thread.messages {
        for msg in messages {
            let role = msg.role.as_deref().unwrap_or("unknown");
            let role_label = match role {
                "user" => "**Human**",
                "assistant" => "**Assistant**",
                _ => role,
            };
            md.push_str(&format!("### {}\n\n", role_label));

            if let Some(ref content) = msg.content {
                for block in content {
                    match block.content_type.as_deref() {
                        Some("text") => {
                            if let Some(ref text) = block.text {
                                md.push_str(text);
                                md.push_str("\n\n");
                            }
                        }
                        Some("thinking") => {
                            if let Some(ref text) = block.text {
                                md.push_str("<details><summary>Thinking</summary>\n\n");
                                md.push_str(text);
                                md.push_str("\n\n</details>\n\n");
                            }
                        }
                        Some("tool_use") => {
                            let tool_name = block.name.as_deref().unwrap_or("tool");
                            md.push_str(&format!("🔧 **Tool: {}**\n\n", tool_name));
                            if let Some(ref input) = block.input {
                                let input_str = serde_json::to_string_pretty(input)
                                    .unwrap_or_else(|_| "...".to_string());
                                if truncate_tool_results && input_str.len() > 500 {
                                    md.push_str(&format!(
                                        "```json\n{}...\n```\n\n",
                                        &input_str[..input_str.floor_char_boundary(500)]
                                    ));
                                } else {
                                    md.push_str(&format!("```json\n{}\n```\n\n", input_str));
                                }
                            }
                        }
                        Some("tool_result") => {
                            if truncate_tool_results {
                                md.push_str("_(tool result truncated)_\n\n");
                            } else if let Some(ref content) = block.content {
                                let content_str = match content {
                                    serde_json::Value::String(s) => s.clone(),
                                    _ => serde_json::to_string_pretty(content)
                                        .unwrap_or_default(),
                                };
                                if content_str.len() > 2000 {
                                    md.push_str(&format!(
                                        "```\n{}...\n```\n\n",
                                        &content_str[..content_str.floor_char_boundary(2000)]
                                    ));
                                } else {
                                    md.push_str(&format!("```\n{}\n```\n\n", content_str));
                                }
                            }
                        }
                        _ => {
                            // Other content types, output as-is if text
                            if let Some(ref text) = block.text {
                                md.push_str(text);
                                md.push_str("\n\n");
                            }
                        }
                    }
                }
            }
        }
    }

    md
}

fn chrono_format_millis(millis: u64) -> String {
    // Simple ISO-ish format without external dependency
    let secs = millis / 1000;
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    // Approximate date calculation
    let mut year = 1970i64;
    let mut remaining_days = days_since_epoch as i64;

    loop {
        let days_in_year = if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_months: [i64; 12] = [
        31,
        if is_leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];

    let mut month = 1;
    for &d in &days_in_months {
        if remaining_days < d {
            break;
        }
        remaining_days -= d;
        month += 1;
    }
    let day = remaining_days + 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}Z",
        year, month, day, hours, minutes
    )
}

// ---------------------------------------------------------------------------
// Handler: /api/internal
// ---------------------------------------------------------------------------

async fn handle_internal_rpc(
    state: &Arc<LocalAmpState>,
    query: Option<&str>,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let method_name = query.unwrap_or("");

    // Try to parse method from query string (e.g. "getUserInfo" or "method=getUserInfo")
    let method = if method_name.contains('=') {
        method_name
            .split('&')
            .find_map(|p| p.strip_prefix("method="))
            .unwrap_or(method_name)
    } else {
        method_name
    };

    // Also try to parse from body
    let body_method = if !body.is_empty() {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(String::from))
    } else {
        None
    };

    let effective_method = if !method.is_empty() {
        method
    } else {
        body_method.as_deref().unwrap_or("")
    };

    match effective_method {
        // ── Auth & user info ────────────────────────────────────────
        "getUserInfo" => handle_get_user_info().await,
        "getUserFreeTierStatus" => {
            // Return "not free tier" so all modes are available
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": {
                    "canUseAmpFree": false,
                    "isDailyGrantEnabled": false
                }
            }))
            .into_response())
        }
        "github-auth-status" => {
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": { "authenticated": true }
            }))
            .into_response())
        }
        "userDisplayBalanceInfo" => {
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": null
            }))
            .into_response())
        }
        "markAsReadMysteriousMessage" => ok_null(),

        // ── Thread CRUD ─────────────────────────────────────────────
        "getThread" => handle_get_thread(state, body).await,
        "listThreads" => handle_list_threads(state, body).await,
        "uploadThread" => handle_upload_thread(state, headers, body).await,
        "setThreadMeta" => handle_set_thread_meta(state, headers, body).await,
        "deleteThread" => handle_delete_thread(state, headers, body).await,
        "getThreadLinkInfo" => {
            // Returns info for rendering a thread link preview.
            // No remote store → empty result.
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": null
            }))
            .into_response())
        }
        "createRemoteExecutorThread" => ok_null(),

        // ── Thread sharing & labels ─────────────────────────────────
        "shareThread" | "shareThreadWithOperator" => handle_share_thread().await,
        "getThreadLabels" => handle_get_thread_labels(body).await,
        "setThreadLabels" => handle_set_thread_labels(body).await,
        "addThreadLabels" => handle_add_thread_labels(body).await,
        "getUserLabels" => {
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": []
            }))
            .into_response())
        }

        // ── Cost / billing ──────────────────────────────────────────
        "threadDisplayCostInfo" => {
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": null
            }))
            .into_response())
        }

        // ── Task management ─────────────────────────────────────────
        "createTask" | "updateTask" | "deleteTask" | "getTask" | "listTasks" => {
            // Tasks are a server-side feature; stub with empty results.
            let result = if effective_method == "listTasks" {
                serde_json::json!([])
            } else {
                serde_json::Value::Null
            };
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": result
            }))
            .into_response())
        }

        // ── Web / external ──────────────────────────────────────────
        "extractWebPageContent" => handle_extract_web_page(state, body).await,
        "webSearch2" => handle_web_search(state, body).await,

        // ── Catch-all ───────────────────────────────────────────────
        _ => {
            tracing::debug!("Unhandled /api/internal method: {}", effective_method);
            ok_null()
        }
    }
}

async fn handle_get_user_info() -> Result<Response, crate::Error> {
    // Return a minimal local user profile
    let device_id = read_device_id().await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "result": {
            "id": device_id,
            "email": "local@localhost",
            "displayName": "Local User",
            "avatarURL": null,
            "features": [],
            "team": null,
            "mysteriousMessage": null
        }
    }))
    .into_response())
}

async fn read_device_id() -> String {
    let path = amp_data_dir().join("device-id.json");
    if let Ok(data) = tokio::fs::read(&path).await
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data)
            && let Some(id) = v.get("installationID").and_then(|i| i.as_str()) {
                return id.to_string();
            }
    "local-user".to_string()
}

async fn handle_share_thread() -> Result<Response, crate::Error> {
    // Stub: pretend sharing succeeded
    Ok(Json(serde_json::json!({
        "ok": true,
        "result": {
            "shared": true,
            "url": null
        }
    }))
    .into_response())
}

/// Shorthand for `{"ok": true, "result": null}` responses.
fn ok_null() -> Result<Response, crate::Error> {
    Ok(Json(serde_json::json!({"ok": true, "result": null})).into_response())
}

/// Handle `getThread` — return thread JSON from local file.
async fn handle_get_thread(
    state: &Arc<LocalAmpState>,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let thread_id = extract_thread_id(body);
    let Some(id) = thread_id else {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "error": { "code": "invalid-request", "message": "missing thread id" }
        }))
        .into_response());
    };

    let path = state.threads_dir.join(format!("{}.json", id));
    match tokio::fs::read(&path).await {
        Ok(data) => {
            // Amp expects: result.thread.data
            let thread: serde_json::Value =
                serde_json::from_slice(&data).unwrap_or(serde_json::Value::Null);
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": { "thread": { "data": thread } }
            })).into_response())
        }
        Err(_) => Ok(Json(serde_json::json!({
            "ok": false,
            "error": { "code": "thread-not-found", "message": format!("Thread {} not found", id) }
        }))
        .into_response()),
    }
}

/// Handle `listThreads` — return summaries of all local threads.
async fn handle_list_threads(
    state: &Arc<LocalAmpState>,
    _body: &Bytes,
) -> Result<Response, crate::Error> {
    state.ensure_index().await;
    let index = state.index.read().await;
    let threads: Vec<serde_json::Value> = index
        .iter()
        .filter(|t| t.message_count > 0) // exclude empty thread stubs
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "v": t.v,
                "title": t.title,
                "created": t.created,
                "updatedAt": t.updated_at,
                "userLastInteractedAt": t.updated_at,
                "messageCount": t.message_count,
                "agentMode": t.agent_mode,
                "archived": t.archived,
                "usesDtw": t.uses_dtw,
                "env": t.env,
                "relationships": t.relationships,
                "summaryStats": {
                    "messageCount": t.message_count,
                    "diffStats": null,
                },
            })
        })
        .collect();
    Ok(Json(serde_json::json!({"ok": true, "result": { "threads": threads }})).into_response())
}

// ---------------------------------------------------------------------------
// Helpers: decompress body if gzip Content-Encoding is present
// ---------------------------------------------------------------------------

/// Amp CLI gzip-compresses request bodies larger than ~25 KB.
/// This helper transparently decompresses when `Content-Encoding: gzip` is set.
fn maybe_decompress(headers: &HeaderMap, body: &Bytes) -> Result<Vec<u8>, String> {
    let is_gzip = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("gzip"))
        .unwrap_or(false);

    if !is_gzip {
        return Ok(body.to_vec());
    }

    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(&body[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("gzip decompression failed: {}", e))?;
    Ok(decompressed)
}

fn parse_rpc_body(
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<serde_json::Value, Response> {
    let raw = maybe_decompress(headers, body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": {"code": "invalid-request", "message": e}})),
        )
            .into_response()
    })?;
    serde_json::from_slice(&raw).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": {"code": "invalid-request", "message": format!("invalid JSON: {}", e)}})),
        )
            .into_response()
    })
}

// ---------------------------------------------------------------------------
// Handler: uploadThread — persist full thread JSON to disk
// ---------------------------------------------------------------------------

async fn handle_upload_thread(
    state: &Arc<LocalAmpState>,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let parsed = match parse_rpc_body(headers, body) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };

    let params = parsed.get("params").unwrap_or(&parsed);
    let thread_value = match params.get("thread") {
        Some(t) if t.is_object() => t,
        _ => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": {"code": "invalid-request", "message": "missing params.thread object"}})),
            )
                .into_response());
        }
    };

    let thread_id = thread_value
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if thread_id.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": {"code": "invalid-request", "message": "thread has no id"}})),
        )
            .into_response());
    }

    // Skip persisting threads with no messages (empty stubs)
    let has_messages = thread_value
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_messages {
        tracing::debug!(thread_id = %thread_id, "Skipping empty thread (no messages)");
        return ok_null();
    }

    // Ensure the threads directory exists
    if let Err(e) = tokio::fs::create_dir_all(&state.threads_dir).await {
        tracing::error!(error = %e, "Failed to create threads directory");
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": {"code": "io-error", "message": format!("failed to create threads dir: {}", e)}})),
        )
            .into_response());
    }

    let path = state.threads_dir.join(format!("{}.json", thread_id));
    let data = serde_json::to_vec(thread_value).unwrap_or_default();
    if let Err(e) = tokio::fs::write(&path, &data).await {
        tracing::error!(thread_id = %thread_id, error = %e, "Failed to write thread file");
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": {"code": "io-error", "message": format!("failed to write thread: {}", e)}})),
        )
            .into_response());
    }

    tracing::debug!(thread_id = %thread_id, size = data.len(), "Wrote thread to disk");
    // Invalidate the in-memory index so the next search picks up the change
    *state.last_indexed.write().await =
        std::time::Instant::now() - std::time::Duration::from_secs(3600);

    ok_null()
}

// ---------------------------------------------------------------------------
// Handler: setThreadMeta — merge meta fields into an existing thread on disk
// ---------------------------------------------------------------------------

async fn handle_set_thread_meta(
    state: &Arc<LocalAmpState>,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let parsed = match parse_rpc_body(headers, body) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };

    let params = parsed.get("params").unwrap_or(&parsed);
    let thread_id = params
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if thread_id.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": {"code": "invalid-request", "message": "missing thread id"}})),
        )
            .into_response());
    }

    let new_meta = match params.get("meta") {
        Some(m) if m.is_object() => m,
        _ => {
            // Nothing to set — just ack
            return ok_null();
        }
    };

    let path = state.threads_dir.join(format!("{}.json", thread_id));
    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(_) => {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "error": { "code": "thread-not-found", "message": format!("Thread {} not found", thread_id) }
            }))
            .into_response());
        }
    };

    let mut thread: serde_json::Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(thread_id = %thread_id, error = %e, "Failed to parse thread for setThreadMeta");
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": {"code": "parse-error", "message": format!("failed to parse thread: {}", e)}})),
            )
                .into_response());
        }
    };

    // Merge new_meta into thread.meta (create if absent)
    if let Some(obj) = thread.as_object_mut() {
        let existing_meta = obj
            .entry("meta")
            .or_insert_with(|| serde_json::json!({}));
        if let (Some(existing), Some(new)) = (existing_meta.as_object_mut(), new_meta.as_object()) {
            for (k, v) in new {
                existing.insert(k.clone(), v.clone());
            }
        }
    }

    let updated = serde_json::to_vec(&thread).unwrap_or_default();
    if let Err(e) = tokio::fs::write(&path, &updated).await {
        tracing::error!(thread_id = %thread_id, error = %e, "Failed to write thread after setThreadMeta");
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": {"code": "io-error", "message": format!("failed to write thread: {}", e)}})),
        )
            .into_response());
    }

    tracing::debug!(thread_id = %thread_id, "Updated thread meta on disk");
    *state.last_indexed.write().await =
        std::time::Instant::now() - std::time::Duration::from_secs(3600);

    ok_null()
}

// ---------------------------------------------------------------------------
// Handler: deleteThread — remove thread file from disk
// ---------------------------------------------------------------------------

async fn handle_delete_thread(
    state: &Arc<LocalAmpState>,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let parsed = match parse_rpc_body(headers, body) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };

    let params = parsed.get("params").unwrap_or(&parsed);
    let thread_id = params
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if thread_id.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": {"code": "invalid-request", "message": "missing thread id"}})),
        )
            .into_response());
    }

    let path = state.threads_dir.join(format!("{}.json", thread_id));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            tracing::debug!(thread_id = %thread_id, "Deleted thread from disk");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Already gone — not an error (matches Amp behavior)
            tracing::debug!(thread_id = %thread_id, "Thread file already absent");
        }
        Err(e) => {
            tracing::error!(thread_id = %thread_id, error = %e, "Failed to delete thread file");
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": {"code": "io-error", "message": format!("failed to delete thread: {}", e)}})),
            )
                .into_response());
        }
    }

    // Also clean up labels for this thread
    let labels_path = amp_data_dir().join("labels.json");
    if let Ok(data) = tokio::fs::read(&labels_path).await {
        if let Ok(mut all) = serde_json::from_slice::<serde_json::Value>(&data) {
            if let Some(obj) = all.as_object_mut() {
                if obj.remove(thread_id).is_some() {
                    if let Ok(updated) = serde_json::to_vec_pretty(&all) {
                        let _ = tokio::fs::write(&labels_path, updated).await;
                    }
                }
            }
        }
    }

    *state.last_indexed.write().await =
        std::time::Instant::now() - std::time::Duration::from_secs(3600);

    ok_null()
}

/// Handle `addThreadLabels` — append labels to existing set.
async fn handle_add_thread_labels(body: &Bytes) -> Result<Response, crate::Error> {
    if body.is_empty() {
        return ok_null();
    }
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let (thread_id, new_labels) = parsed
        .as_ref()
        .map(|v| {
            let params = v.get("params").unwrap_or(v);
            let id = params
                .get("thread")
                .and_then(|t| t.as_str())
                .map(String::from);
            let labels = params
                .get("labels")
                .and_then(|l| l.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (id, labels)
        })
        .unwrap_or((None, vec![]));

    if let Some(ref id) = thread_id {
        // Read existing labels, merge, write back
        let existing = read_local_labels(id).await;
        let mut all: Vec<String> = existing
            .iter()
            .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        for label in &new_labels {
            if !all.contains(label) {
                all.push(label.clone());
            }
        }
        write_local_labels(id, &all).await;
    }
    ok_null()
}

/// Extract a thread ID from a JSON body (checks `params.thread` and `thread`).
fn extract_thread_id(body: &Bytes) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("params")
        .and_then(|p| p.get("thread").or_else(|| p.get("threadID")))
        .or_else(|| v.get("thread").or_else(|| v.get("threadID")))
        .and_then(|t| t.as_str())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Handlers: extractWebPageContent / webSearch2 — delegate to WebBackend
// ---------------------------------------------------------------------------

async fn handle_extract_web_page(
    state: &Arc<LocalAmpState>,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let parsed: serde_json::Value = if !body.is_empty() {
        serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::Value::Null
    };
    let params = parsed.get("params").unwrap_or(&parsed);
    let url = params
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or_default();

    if url.is_empty() {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "error": { "code": "invalid-request", "message": "missing url" }
        }))
        .into_response());
    }

    match state.web.extract_page(url.to_string()).await {
        Ok(page) => Ok(Json(serde_json::json!({
            "ok": true,
            "result": {
                "fullContent": page.full_content,
                "excerpts": page.excerpts,
            }
        }))
        .into_response()),
        Err(e) => {
            tracing::warn!("Page extraction failed for {}: {}", url, e);
            Ok(Json(serde_json::json!({
                "ok": false,
                "error": { "code": "upstream-error", "message": e }
            }))
            .into_response())
        }
    }
}

async fn handle_web_search(
    state: &Arc<LocalAmpState>,
    body: &Bytes,
) -> Result<Response, crate::Error> {
    let parsed: serde_json::Value = if !body.is_empty() {
        serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::Value::Null
    };
    let params = parsed.get("params").unwrap_or(&parsed);

    let queries: Vec<&str> = params
        .get("searchQueries")
        .and_then(|q| q.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let objective = params
        .get("objective")
        .and_then(|o| o.as_str())
        .unwrap_or("");
    let max_results = params
        .get("maxResults")
        .and_then(|m| m.as_u64())
        .unwrap_or(5) as usize;

    let search_terms: Vec<String> = if queries.is_empty() {
        if objective.is_empty() {
            vec![]
        } else {
            vec![objective.to_string()]
        }
    } else {
        queries.into_iter().map(|s| s.to_string()).collect()
    };

    match state.web.search(search_terms, max_results).await {
        Ok(results) => Ok(Json(serde_json::json!({
            "ok": true,
            "result": {
                "results": results,
                "showParallelAttribution": false,
            }
        }))
        .into_response()),
        Err(e) => {
            tracing::warn!("Web search failed: {}", e);
            Ok(Json(serde_json::json!({
                "ok": true,
                "result": {
                    "results": [],
                    "showParallelAttribution": false,
                }
            }))
            .into_response())
        }
    }
}

async fn handle_get_thread_labels(body: &Bytes) -> Result<Response, crate::Error> {
    // Parse the thread ID from the request
    let thread_id = if !body.is_empty() {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("params")
                    .and_then(|p| p.get("thread").and_then(|t| t.as_str()).map(String::from))
                    .or_else(|| v.get("thread").and_then(|t| t.as_str()).map(String::from))
            })
    } else {
        None
    };

    // Read labels from local file if it exists
    let labels = if let Some(ref id) = thread_id {
        read_local_labels(id).await
    } else {
        vec![]
    };

    Ok(Json(serde_json::json!({
        "ok": true,
        "result": labels
    }))
    .into_response())
}

async fn handle_set_thread_labels(body: &Bytes) -> Result<Response, crate::Error> {
    if body.is_empty() {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "result": null
        }))
        .into_response());
    }

    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let (thread_id, labels) = parsed
        .as_ref()
        .map(|v| {
            let params = v.get("params").unwrap_or(v);
            let id = params
                .get("thread")
                .and_then(|t| t.as_str())
                .map(String::from);
            let labels = params
                .get("labels")
                .and_then(|l| l.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (id, labels)
        })
        .unwrap_or((None, vec![]));

    if let Some(ref id) = thread_id {
        write_local_labels(id, &labels).await;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "result": null
    }))
    .into_response())
}

/// Labels are stored in a simple JSON file next to the threads
async fn read_local_labels(thread_id: &str) -> Vec<serde_json::Value> {
    let path = amp_data_dir().join("labels.json");
    if let Ok(data) = tokio::fs::read(&path).await
        && let Ok(all) = serde_json::from_slice::<serde_json::Value>(&data)
            && let Some(labels) = all.get(thread_id).and_then(|l| l.as_array()) {
                return labels
                    .iter()
                    .filter_map(|l| {
                        l.as_str()
                            .map(|name| serde_json::json!({"name": name}))
                    })
                    .collect();
            }
    vec![]
}

async fn write_local_labels(thread_id: &str, labels: &[String]) {
    let path = amp_data_dir().join("labels.json");
    let mut all: serde_json::Value = if let Ok(data) = tokio::fs::read(&path).await {
        serde_json::from_slice(&data).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = all.as_object_mut() {
        let arr: Vec<serde_json::Value> = labels
            .iter()
            .map(|l| serde_json::Value::String(l.clone()))
            .collect();
        obj.insert(thread_id.to_string(), serde_json::Value::Array(arr));
    }

    if let Ok(data) = serde_json::to_vec_pretty(&all) {
        let _ = tokio::fs::write(&path, data).await;
    }
}

// ---------------------------------------------------------------------------
// Handler: /api/durable-thread-workers/{id}
// ---------------------------------------------------------------------------

async fn handle_durable_thread_workers(path: &str) -> Result<Response, crate::Error> {
    let id = path
        .strip_prefix("durable-thread-workers/")
        .unwrap_or("unknown");
    // Stub: return a minimal "created" response
    Ok(Json(serde_json::json!({
        "id": id,
        "status": "running",
        "executorType": "local-client"
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Handler: /api/users/{id}
// ---------------------------------------------------------------------------

async fn handle_user_info() -> Result<Response, crate::Error> {
    let device_id = read_device_id().await;
    Ok(Json(serde_json::json!({
        "id": device_id,
        "email": "local@localhost",
        "displayName": "Local User",
        "avatarURL": null
    }))
    .into_response())
}
