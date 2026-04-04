//! Tavily Search API backend.
//!
//! Requires `TAVILY_API_KEY` environment variable.

use super::{PageContent, SearchResult, WebBackend};
use reqwest::Client;
use std::future::Future;
use std::pin::Pin;

pub struct TavilyBackend {
    http: Client,
}

impl TavilyBackend {
    pub fn new(http: Client) -> Self {
        Self { http }
    }
}

fn api_key() -> Result<String, String> {
    std::env::var("TAVILY_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| "TAVILY_API_KEY not set".to_string())
}

impl WebBackend for TavilyBackend {
    fn search(
        &self,
        queries: Vec<String>,
        max_results: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchResult>, String>> + Send + '_>> {
        Box::pin(async move {
            let key = api_key()?;
            let mut results = Vec::new();

            for query in &queries {
                if query.is_empty() {
                    continue;
                }
                let body = serde_json::json!({
                    "api_key": key,
                    "query": query,
                    "max_results": max_results.min(10),
                    "include_answer": false,
                });

                match self.http.post("https://api.tavily.com/search").json(&body).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let body: serde_json::Value =
                            resp.json().await.unwrap_or(serde_json::Value::Null);
                        if let Some(items) = body.get("results").and_then(|r| r.as_array()) {
                            for item in items {
                                results.push(SearchResult {
                                    title: item.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                                    url: item.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
                                    content: item.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string(),
                                });
                            }
                        }
                    }
                    Ok(resp) => tracing::warn!("Tavily search failed for {:?}: {}", query, resp.status()),
                    Err(e) => tracing::warn!("Tavily request failed for {:?}: {}", query, e),
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
        Box::pin(async move {
            let key = api_key()?;
            let body = serde_json::json!({ "api_key": key, "urls": [url] });

            let resp = self
                .http
                .post("https://api.tavily.com/extract")
                .json(&body)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            if !resp.status().is_success() {
                return Err(format!("Tavily extract returned {}", resp.status()));
            }

            let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            let content = body
                .get("results")
                .and_then(|r| r.get(0))
                .and_then(|r| r.get("raw_content"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();

            Ok(PageContent {
                full_content: content,
                excerpts: vec![],
            })
        })
    }
}
