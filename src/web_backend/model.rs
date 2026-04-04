//! Model-native web search via Copilot chat/completions with `web_search_preview`.

use super::{PageContent, SearchResult, WebBackend};
use crate::proxy::ProxyClient;
use axum::body::Bytes;
use reqwest::Client;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct ModelBackend {
    http: Client,
    proxy: Arc<ProxyClient>,
}

impl ModelBackend {
    pub fn new(http: Client, proxy: Arc<ProxyClient>) -> Self {
        Self { http, proxy }
    }

    async fn search_via_model(&self, query: &str) -> Result<String, String> {
        let body = serde_json::json!({
            "model": "gpt-4o-mini",
            "messages": [
                {
                    "role": "system",
                    "content": "You are a helpful search assistant. Return search results as a JSON array of objects with 'title', 'url', and 'content' fields. Return ONLY the JSON array, no other text."
                },
                {
                    "role": "user",
                    "content": format!("Search the web for: {}", query)
                }
            ],
            "tools": [
                { "type": "web_search_preview" }
            ],
            "tool_choice": "auto"
        });

        let body_bytes = Bytes::from(serde_json::to_vec(&body).map_err(|e| e.to_string())?);

        let resp = self
            .proxy
            .forward(
                "/chat/completions",
                reqwest::Method::POST,
                body_bytes,
                Some("application/json"),
                Some("agent"),
                false,
            )
            .await
            .map_err(|e| format!("Copilot request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Copilot returned {}: {}",
                status,
                &text[..text.floor_char_boundary(200)]
            ));
        }

        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

        let content = body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        Ok(content)
    }
}

impl WebBackend for ModelBackend {
    fn search(
        &self,
        queries: Vec<String>,
        max_results: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchResult>, String>> + Send + '_>> {
        Box::pin(async move {
            let mut results = Vec::new();

            for query in &queries {
                if query.is_empty() {
                    continue;
                }
                match self.search_via_model(query).await {
                    Ok(text) => results.extend(parse_search_results(&text)),
                    Err(e) => tracing::warn!("Model search failed for {:?}: {}", query, e),
                }
                if results.len() >= max_results {
                    break;
                }
            }

            results.truncate(max_results);
            Ok(results)
        })
    }

    fn extract_page(
        &self,
        url: String,
    ) -> Pin<Box<dyn Future<Output = Result<PageContent, String>> + Send + '_>> {
        // Model-native search doesn't have page extraction — fall back to Jina.
        Box::pin(async move {
            let jina = super::jina::JinaBackend::new(self.http.clone());
            jina.do_extract_page(&url).await
        })
    }
}

/// Try to extract `SearchResult`s from model response text.
fn parse_search_results(text: &str) -> Vec<SearchResult> {
    // Strategy 1: direct JSON parse
    if let Some(r) = try_parse_json_array(text) {
        return r;
    }

    // Strategy 2: find outermost [ ... ]
    let trimmed = text.trim();
    if let Some(start) = trimmed.find('[')
        && let Some(end) = trimmed.rfind(']')
            && start < end
                && let Some(r) = try_parse_json_array(&trimmed[start..=end]) {
                    return r;
                }

    // Strategy 3: try ```json ... ``` blocks
    for block in text.split("```") {
        let block = block.strip_prefix("json").unwrap_or(block).trim();
        if block.starts_with('[')
            && let Some(r) = try_parse_json_array(block) {
                return r;
            }
    }

    vec![]
}

fn try_parse_json_array(text: &str) -> Option<Vec<SearchResult>> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(text).ok()?;
    let results: Vec<SearchResult> = arr
        .into_iter()
        .filter_map(|item| {
            Some(SearchResult {
                title: item.get("title")?.as_str()?.to_string(),
                url: item.get("url")?.as_str()?.to_string(),
                content: item
                    .get("content")
                    .or_else(|| item.get("description"))
                    .or_else(|| item.get("snippet"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    if results.is_empty() { None } else { Some(results) }
}
