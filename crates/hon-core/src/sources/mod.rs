use std::time::Duration;

use crate::model::{ChapterRef, MangaRef, PageRef, SourceId};

pub const USER_AGENT: &str = concat!("hon/", env!("CARGO_PKG_VERSION"));
/// For sites that turn away clients that don't look like a browser.
pub const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>>;
    /// Hosted English chapters only, sorted ascending, one entry per ChapterNo.
    async fn chapters(&self, manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>>;
    async fn pages(&self, chapter: &ChapterRef) -> anyhow::Result<Vec<PageRef>>;
    async fn fetch_page(&self, page: &PageRef) -> anyhow::Result<Vec<u8>>;
}

pub fn http_client() -> reqwest::Client {
    http_client_builder()
        .build()
        .expect("static reqwest client config is valid")
}

/// The timeout, user agent and compression every hon client uses.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(USER_AGENT)
        .gzip(true)
}
