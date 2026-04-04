//! Pluggable web search and page extraction backends.
//!
//! Each backend implements [`WebBackend`] which provides `search()` and
//! `extract_page()`.  The active backend is selected via `--search-provider`.

pub mod brave;
pub mod jina;
pub mod model;
pub mod none;
pub mod searxng;
pub mod tavily;

use std::fmt;
use std::future::Future;
use std::pin::Pin;

// ---------------------------------------------------------------------------
// Shared result type
// ---------------------------------------------------------------------------

/// A single search result returned to amp.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub content: String,
}

/// Page extraction result.
pub struct PageContent {
    pub full_content: String,
    pub excerpts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Trait  (dyn-compatible via boxed futures)
// ---------------------------------------------------------------------------

pub trait WebBackend: Send + Sync {
    fn search(
        &self,
        queries: Vec<String>,
        max_results: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchResult>, String>> + Send + '_>>;

    fn extract_page(
        &self,
        url: String,
    ) -> Pin<Box<dyn Future<Output = Result<PageContent, String>> + Send + '_>>;
}

// ---------------------------------------------------------------------------
// Provider enum + factory
// ---------------------------------------------------------------------------

/// CLI-selectable search provider.
#[derive(Clone, Debug, Default)]
pub enum SearchProvider {
    #[default]
    Jina,
    Tavily,
    Brave,
    Searxng,
    Model,
    None,
}

impl fmt::Display for SearchProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jina => write!(f, "jina"),
            Self::Tavily => write!(f, "tavily"),
            Self::Brave => write!(f, "brave"),
            Self::Searxng => write!(f, "searxng"),
            Self::Model => write!(f, "model"),
            Self::None => write!(f, "none"),
        }
    }
}

impl std::str::FromStr for SearchProvider {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "jina" => Ok(Self::Jina),
            "tavily" => Ok(Self::Tavily),
            "brave" => Ok(Self::Brave),
            "searxng" => Ok(Self::Searxng),
            "model" => Ok(Self::Model),
            "none" => Ok(Self::None),
            other => Err(format!(
                "unknown search provider '{}' (expected: jina, tavily, brave, searxng, model, none)",
                other
            )),
        }
    }
}

/// Build a [`WebBackend`] from the selected provider.
pub fn create_backend(
    provider: &SearchProvider,
    http: reqwest::Client,
    proxy: Option<std::sync::Arc<crate::proxy::ProxyClient>>,
    search_model: Option<String>,
) -> Box<dyn WebBackend> {
    match provider {
        SearchProvider::Jina => Box::new(jina::JinaBackend::new(http)),
        SearchProvider::Tavily => Box::new(tavily::TavilyBackend::new(http)),
        SearchProvider::Brave => Box::new(brave::BraveBackend::new(http)),
        SearchProvider::Searxng => Box::new(searxng::SearxngBackend::new(http)),
        SearchProvider::Model => {
            let proxy =
                proxy.expect("--search-provider=model requires the Copilot proxy to be running");
            let model = search_model.unwrap_or_else(|| "gpt-5-mini".to_string());
            Box::new(model::ModelBackend::new(http, proxy, model))
        }
        SearchProvider::None => Box::new(none::NoneBackend),
    }
}
