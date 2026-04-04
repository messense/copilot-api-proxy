//! Brave Search API backend.
//!
//! Requires `BRAVE_API_KEY` environment variable.
//! No page-extract endpoint — falls back to Jina Reader.

use super::{PageContent, SearchResult, WebBackend};
use reqwest::Client;
use std::future::Future;
use std::pin::Pin;

pub struct BraveBackend {
    http: Client,
}

impl BraveBackend {
    pub fn new(http: Client) -> Self {
        Self { http }
    }
}

fn api_key() -> Result<String, String> {
    std::env::var("BRAVE_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| "BRAVE_API_KEY not set".to_string())
}

impl WebBackend for BraveBackend {
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

                let url = format!(
                    "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
                    urlencoding::encode(query),
                    max_results.min(20)
                );

                let resp = self
                    .http
                    .get(&url)
                    .header("X-Subscription-Token", &key)
                    .header("Accept", "application/json")
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value =
                            r.json().await.unwrap_or(serde_json::Value::Null);
                        if let Some(items) = body
                            .get("web")
                            .and_then(|w| w.get("results"))
                            .and_then(|r| r.as_array())
                        {
                            for item in items {
                                results.push(SearchResult {
                                    title: item.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                                    url: item.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
                                    content: item.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                                });
                            }
                        }
                    }
                    Ok(r) => tracing::warn!("Brave search failed for {:?}: {}", query, r.status()),
                    Err(e) => tracing::warn!("Brave request failed for {:?}: {}", query, e),
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
            let jina = super::jina::JinaBackend::new(self.http.clone());
            jina.do_extract_page(&url).await
        })
    }
}
