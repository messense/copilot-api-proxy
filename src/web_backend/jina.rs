//! Jina Reader (`r.jina.ai`) + Jina Search (`s.jina.ai`) backend.

use super::{PageContent, SearchResult, WebBackend};
use reqwest::Client;
use std::future::Future;
use std::pin::Pin;

pub struct JinaBackend {
    http: Client,
}

impl JinaBackend {
    pub fn new(http: Client) -> Self {
        Self { http }
    }

    pub async fn do_extract_page(&self, url: &str) -> Result<PageContent, String> {
        let jina_url = format!("https://r.jina.ai/{}", url);
        let mut req = self
            .http
            .get(&jina_url)
            .header("Accept", "application/json")
            .header("X-No-Cache", "true");
        if let Some(auth) = jina_auth_header() {
            req = req.header("Authorization", &auth);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("Jina Reader returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let content = body
            .get("data")
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        Ok(PageContent {
            full_content: content,
            excerpts: vec![],
        })
    }
}

fn jina_auth_header() -> Option<String> {
    std::env::var("JINA_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .map(|k| format!("Bearer {}", k))
}

impl WebBackend for JinaBackend {
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
                let url = format!("https://s.jina.ai/{}", urlencoding::encode(query));
                let mut req = self.http.get(&url).header("Accept", "application/json");
                if let Some(auth) = jina_auth_header() {
                    req = req.header("Authorization", &auth);
                }

                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let body: serde_json::Value =
                            resp.json().await.unwrap_or(serde_json::Value::Null);
                        if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
                            for item in data {
                                results.push(SearchResult {
                                    title: item
                                        .get("title")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    url: item
                                        .get("url")
                                        .and_then(|u| u.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    content: item
                                        .get("content")
                                        .or_else(|| item.get("description"))
                                        .and_then(|c| c.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                });
                            }
                        }
                    }
                    Ok(resp) => {
                        tracing::warn!("Jina Search failed for {:?}: {}", query, resp.status());
                    }
                    Err(e) => {
                        tracing::warn!("Jina Search request failed for {:?}: {}", query, e);
                    }
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
        Box::pin(async move { self.do_extract_page(&url).await })
    }
}
