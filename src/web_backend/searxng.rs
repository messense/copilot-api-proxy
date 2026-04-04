//! SearXNG backend — self-hosted meta-search engine.
//!
//! Requires `SEARXNG_URL` environment variable (e.g. `http://localhost:8080`).
//! No page-extract support — falls back to Jina Reader.

use super::{PageContent, SearchResult, WebBackend};
use reqwest::Client;
use std::future::Future;
use std::pin::Pin;

pub struct SearxngBackend {
    http: Client,
}

impl SearxngBackend {
    pub fn new(http: Client) -> Self {
        Self { http }
    }
}

fn base_url() -> Result<String, String> {
    std::env::var("SEARXNG_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .map(|u| u.trim_end_matches('/').to_string())
        .ok_or_else(|| "SEARXNG_URL not set".to_string())
}

impl WebBackend for SearxngBackend {
    fn search(
        &self,
        queries: Vec<String>,
        max_results: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchResult>, String>> + Send + '_>> {
        Box::pin(async move {
            let base = base_url()?;
            let mut results = Vec::new();

            for query in &queries {
                if query.is_empty() {
                    continue;
                }

                let url = format!(
                    "{}/search?q={}&format=json",
                    base,
                    urlencoding::encode(query)
                );

                let resp = self.http.get(&url).send().await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value =
                            r.json().await.unwrap_or(serde_json::Value::Null);
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
                    Ok(r) => tracing::warn!("SearXNG search failed for {:?}: {}", query, r.status()),
                    Err(e) => tracing::warn!("SearXNG request failed for {:?}: {}", query, e),
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
