pub mod mangadex;
pub mod mangakatana;
pub mod weebcentral;

use std::sync::Arc;
use std::time::Duration;

use crate::model::{ChapterRef, MangaRef, PageRef, SourceId};

pub const USER_AGENT: &str = concat!("hon/", env!("CARGO_PKG_VERSION"));

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
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(USER_AGENT)
        .gzip(true)
        .build()
        .expect("static reqwest client config is valid")
}

/// The one registration list of sources.
pub fn all(http: reqwest::Client) -> Vec<Arc<dyn Source>> {
    vec![
        Arc::new(mangadex::MangaDex::new(
            http.clone(),
            "https://api.mangadex.org",
        )),
        Arc::new(mangakatana::MangaKatana::new(
            http.clone(),
            "https://mangakatana.com",
        )),
        Arc::new(weebcentral::WeebCentral::new(
            http,
            "https://weebcentral.com",
        )),
    ]
}
