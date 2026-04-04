//! No-op backend — returns empty results for everything.

use super::{PageContent, SearchResult, WebBackend};
use std::future::Future;
use std::pin::Pin;

pub struct NoneBackend;

impl WebBackend for NoneBackend {
    fn search(
        &self,
        _queries: Vec<String>,
        _max_results: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchResult>, String>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn extract_page(
        &self,
        _url: String,
    ) -> Pin<Box<dyn Future<Output = Result<PageContent, String>> + Send + '_>> {
        Box::pin(async {
            Ok(PageContent {
                full_content: String::new(),
                excerpts: vec![],
            })
        })
    }
}
