use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::Context;
use futures::future::BoxFuture;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::model::{ChapterNo, ChapterRef, MangaRef, PageRef, SourceId};
use crate::sources::{Source, USER_AGENT};

const MIN_API_GAP: Duration = Duration::from_millis(200);
const FEED_LIMIT: u64 = 500;
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(5);
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

type Wait = Box<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct MangaDex {
    http: reqwest::Client,
    api_base: String,
    last_call: Mutex<Instant>,
    /// Waits out a 429; replaced in tests so they don't sleep.
    rate_limit_wait: Wait,
}

impl MangaDex {
    pub fn new(http: reqwest::Client, api_base: &str) -> MangaDex {
        let past = Instant::now()
            .checked_sub(MIN_API_GAP)
            .unwrap_or_else(Instant::now);
        MangaDex {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            last_call: Mutex::new(past),
            rate_limit_wait: Box::new(|d| Box::pin(tokio::time::sleep(d))),
        }
    }

    #[cfg(test)]
    fn waiting_with(
        mut self,
        wait: impl Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> MangaDex {
        self.rate_limit_wait = Box::new(wait);
        self
    }

    async fn api_send(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> anyhow::Result<reqwest::Response> {
        {
            let mut last = self.last_call.lock().await;
            tokio::time::sleep_until(*last + MIN_API_GAP).await;
            *last = Instant::now();
        }
        let url = format!("{}{path}", self.api_base);
        self.http
            .get(&url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .query(query)
            .send()
            .await
            .with_context(|| format!("MangaDex request to {path} failed"))
    }

    async fn api_get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> anyhow::Result<T> {
        parse(self.api_send(path, query).await?, path).await
    }
}

async fn parse<T: DeserializeOwned>(resp: reqwest::Response, path: &str) -> anyhow::Result<T> {
    resp.error_for_status()
        .with_context(|| format!("MangaDex returned an error for {path}"))?
        .json()
        .await
        .with_context(|| format!("MangaDex sent unexpected data for {path}"))
}

/// How long a 429 asks to wait: `Retry-After` seconds, else MangaDex's
/// `X-RateLimit-Retry-After` Unix time; 5 s without either, never more than 60 s.
fn retry_wait(headers: &HeaderMap, now: i64) -> Duration {
    let number =
        |name: &str| -> Option<i64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    let secs = number("retry-after").or_else(|| Some(number("x-ratelimit-retry-after")? - now));
    secs.map_or(RATE_LIMIT_WAIT, |s| {
        Duration::from_secs(s.max(0) as u64).min(MAX_RATE_LIMIT_WAIT)
    })
}

#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<MangaItem>,
}

#[derive(Deserialize)]
struct MangaItem {
    id: String,
    attributes: MangaAttributes,
}

#[derive(Deserialize)]
struct MangaAttributes {
    title: BTreeMap<String, String>,
    #[serde(rename = "altTitles", default)]
    alt_titles: Vec<BTreeMap<String, String>>,
}

impl MangaItem {
    fn into_manga_ref(self) -> Option<MangaRef> {
        let MangaAttributes { title, alt_titles } = self.attributes;
        let alt_en = alt_titles.iter().filter_map(|t| t.get("en"));
        let display = title
            .get("en")
            .or_else(|| alt_en.clone().next())
            .or_else(|| title.values().next())?
            .clone();
        let mut alts: Vec<String> = Vec::new();
        for t in alt_en.chain(title.values()) {
            if !alts.contains(t) {
                alts.push(t.clone());
            }
        }
        Some(MangaRef {
            source: SourceId::MangaDex,
            url: format!("https://mangadex.org/title/{}", self.id),
            id: self.id,
            title: display,
            alt_titles: alts,
        })
    }
}

#[derive(Deserialize)]
struct FeedResponse {
    data: Vec<ChapterItem>,
    total: u64,
}

#[derive(Deserialize)]
struct ChapterItem {
    id: String,
    attributes: ChapterAttributes,
}

#[derive(Deserialize)]
struct ChapterAttributes {
    chapter: Option<String>,
    title: Option<String>,
    #[serde(rename = "externalUrl")]
    external_url: Option<String>,
    #[serde(default)]
    pages: u32,
    #[serde(rename = "publishAt")]
    publish_at: Option<String>,
}

#[derive(Deserialize)]
struct AtHomeResponse {
    #[serde(rename = "baseUrl")]
    base_url: String,
    chapter: AtHomeChapter,
}

#[derive(Deserialize)]
struct AtHomeChapter {
    hash: String,
    data: Vec<String>,
}

#[async_trait::async_trait]
impl Source for MangaDex {
    fn id(&self) -> SourceId {
        SourceId::MangaDex
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>> {
        let resp: SearchResponse = self
            .api_get(
                "/manga",
                &[
                    ("title", query.to_string()),
                    ("limit", "20".into()),
                    ("availableTranslatedLanguage[]", "en".into()),
                    ("order[relevance]", "desc".into()),
                ],
            )
            .await?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(MangaItem::into_manga_ref)
            .collect())
    }

    async fn chapters(&self, manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
        // Several groups may upload the same number: keep the one with the most pages.
        let mut best: HashMap<ChapterNo, (u32, ChapterRef)> = HashMap::new();
        let mut offset = 0;
        loop {
            let resp: FeedResponse = self
                .api_get(
                    &format!("/manga/{}/feed", manga.id),
                    &[
                        ("translatedLanguage[]", "en".into()),
                        ("order[chapter]", "asc".into()),
                        ("limit", FEED_LIMIT.to_string()),
                        ("offset", offset.to_string()),
                        ("includeExternalUrl", "0".into()),
                    ],
                )
                .await?;
            for item in resp.data {
                let a = item.attributes;
                if a.external_url.is_some() || a.pages == 0 {
                    continue;
                }
                let number = match a.chapter.as_deref() {
                    None => ChapterNo::parse("0"),
                    Some(s) => ChapterNo::parse(s),
                };
                let Some(number) = number else { continue };
                if best
                    .get(&number)
                    .is_some_and(|(pages, _)| *pages >= a.pages)
                {
                    continue;
                }
                let chapter = ChapterRef {
                    source: SourceId::MangaDex,
                    id: item.id,
                    number,
                    title: a.title.filter(|t| !t.trim().is_empty()),
                    published: a.publish_at.and_then(|p| p.parse().ok()),
                };
                best.insert(number, (a.pages, chapter));
            }
            offset += FEED_LIMIT;
            if offset >= resp.total {
                break;
            }
        }
        let mut chapters: Vec<ChapterRef> = best.into_values().map(|(_, c)| c).collect();
        chapters.sort_by_key(|c| c.number);
        Ok(chapters)
    }

    async fn pages(&self, chapter: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
        // The at-home endpoint has its own tight rate limit: wait it out once.
        let path = format!("/at-home/server/{}", chapter.id);
        let mut sent = self.api_send(&path, &[]).await?;
        if sent.status() == StatusCode::TOO_MANY_REQUESTS {
            let now = jiff::Timestamp::now().as_second();
            (self.rate_limit_wait)(retry_wait(sent.headers(), now)).await;
            sent = self.api_send(&path, &[]).await?;
        }
        let resp: AtHomeResponse = parse(sent, &path).await?;
        let base = resp.base_url.trim_end_matches('/');
        Ok(resp
            .chapter
            .data
            .iter()
            .map(|file| PageRef {
                url: format!("{base}/data/{}/{file}", resp.chapter.hash),
            })
            .collect())
    }

    async fn fetch_page(&self, page: &PageRef) -> anyhow::Result<Vec<u8>> {
        let bytes = self
            .http
            .get(&page.url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .with_context(|| format!("fetching page {} failed", page.url))?
            .error_for_status()
            .with_context(|| format!("page request returned an error: {}", page.url))?
            .bytes()
            .await
            .with_context(|| format!("reading page {} failed", page.url))?;
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use super::*;
    use crate::model::{ChapterNo, ChapterRef, MangaRef, SourceId};
    use crate::sources::{Source, USER_AGENT};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SEARCH: &str = include_str!("../../tests/fixtures/mangadex/search.json");
    const FEED: &str = include_str!("../../tests/fixtures/mangadex/feed_page1.json");
    const AT_HOME: &str = include_str!("../../tests/fixtures/mangadex/at_home.json");

    fn json(body: impl Into<String>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.into(), "application/json")
    }

    fn manga() -> MangaRef {
        MangaRef {
            source: SourceId::MangaDex,
            id: "m1".into(),
            title: "T".into(),
            alt_titles: vec![],
            url: String::new(),
        }
    }

    #[tokio::test]
    async fn search_prefers_english_title() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manga"))
            .and(query_param("title", "solo leveling"))
            .respond_with(json(SEARCH))
            .mount(&server)
            .await;
        let md = MangaDex::new(reqwest::Client::new(), &server.uri());
        let results = md.search("solo leveling").await.unwrap();
        assert!(results.len() >= 2);
        assert_eq!(results[0].title, "Solo Leveling");
        assert!(
            results[0]
                .alt_titles
                .contains(&"Na Honjaman Level-Up".to_string())
        );
        assert_eq!(results[0].source, SourceId::MangaDex);
        assert_eq!(
            results[0].url,
            format!("https://mangadex.org/title/{}", results[0].id)
        );
        assert_eq!(results[1].title, "Solo Leveling: Arise");
    }

    #[tokio::test]
    async fn chapters_skip_external_dedupe_and_paginate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manga/m1/feed"))
            .and(query_param("offset", "0"))
            .and(query_param("includeExternalUrl", "0"))
            .respond_with(json(FEED))
            .expect(1)
            .mount(&server)
            .await;
        let page2 = serde_json::json!({
            "result": "ok", "limit": 500, "offset": 500, "total": 501,
            "data": [{"id": "p2-ch11", "attributes": {
                "chapter": "11", "title": "Eleven", "externalUrl": null,
                "pages": 9, "publishAt": "2024-02-01T00:00:00+00:00"
            }}]
        });
        Mock::given(method("GET"))
            .and(path("/manga/m1/feed"))
            .and(query_param("offset", "500"))
            .respond_with(json(page2.to_string()))
            .expect(1)
            .mount(&server)
            .await;

        let md = MangaDex::new(reqwest::Client::new(), &server.uri());
        let chapters = md.chapters(&manga()).await.unwrap();
        let numbers: Vec<String> = chapters.iter().map(|c| c.number.to_string()).collect();
        assert_eq!(numbers, ["0", "5", "10.5", "11"]);
        let ch5 = chapters
            .iter()
            .find(|c| c.number.to_string() == "5")
            .unwrap();
        assert_eq!(ch5.id, "ch5-22pages");
        assert!(chapters.iter().all(|c| c.id != "ext-0000"));
        assert_eq!(chapters[3].title.as_deref(), Some("Eleven"));
        assert_eq!(
            chapters[3].published,
            Some("2024-02-01T00:00:00Z".parse().unwrap())
        );
        assert!(chapters.iter().all(|c| c.source == SourceId::MangaDex));
    }

    #[tokio::test]
    async fn pages_build_at_home_urls() {
        let server = MockServer::start().await;
        let v: serde_json::Value = serde_json::from_str(AT_HOME).unwrap();
        let hash = v["chapter"]["hash"].as_str().unwrap().to_string();
        let first = v["chapter"]["data"][0].as_str().unwrap().to_string();
        let body = AT_HOME.replace(v["baseUrl"].as_str().unwrap(), &server.uri());
        Mock::given(method("GET"))
            .and(path("/at-home/server/ch1"))
            .respond_with(json(body))
            .mount(&server)
            .await;
        let md = MangaDex::new(reqwest::Client::new(), &server.uri());
        let chapter = ChapterRef {
            source: SourceId::MangaDex,
            id: "ch1".into(),
            number: ChapterNo::parse("1").unwrap(),
            title: None,
            published: None,
        };
        let pages = md.pages(&chapter).await.unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(
            pages[0].url,
            format!("{}/data/{hash}/{first}", server.uri())
        );
    }

    fn chapter(id: &str) -> ChapterRef {
        ChapterRef {
            source: SourceId::MangaDex,
            id: id.into(),
            number: ChapterNo::parse("1").unwrap(),
            title: None,
            published: None,
        }
    }

    /// A MangaDex that records its rate-limit waits instead of sleeping.
    fn recording(server: &MockServer) -> (MangaDex, Arc<StdMutex<Vec<Duration>>>) {
        let waits = Arc::new(StdMutex::new(Vec::new()));
        let recorded = waits.clone();
        let md = MangaDex::new(reqwest::Client::new(), &server.uri()).waiting_with(move |d| {
            recorded.lock().unwrap().push(d);
            Box::pin(async {})
        });
        (md, waits)
    }

    #[tokio::test]
    async fn at_home_429_retried_once_after_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/at-home/server/ch1"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/at-home/server/ch1"))
            .respond_with(json(AT_HOME))
            .expect(1)
            .mount(&server)
            .await;
        let (md, waits) = recording(&server);
        assert_eq!(md.pages(&chapter("ch1")).await.unwrap().len(), 3);
        assert_eq!(*waits.lock().unwrap(), [Duration::from_secs(7)]);
    }

    #[tokio::test]
    async fn at_home_429_twice_is_an_error() {
        let server = MockServer::start().await;
        let reset = jiff::Timestamp::now().as_second() + 600;
        Mock::given(method("GET"))
            .and(path("/at-home/server/ch1"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("X-RateLimit-Retry-After", reset.to_string().as_str()),
            )
            .expect(2)
            .mount(&server)
            .await;
        let (md, waits) = recording(&server);
        let err = md.pages(&chapter("ch1")).await.unwrap_err();
        assert!(format!("{err:#}").contains("429"), "{err:#}");
        assert_eq!(*waits.lock().unwrap(), [Duration::from_secs(60)], "capped");
    }

    #[test]
    fn retry_wait_from_headers() {
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut h = reqwest::header::HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, v.parse().unwrap());
            }
            h
        };
        let now = 1_800_000_000;
        let wait = |pairs: &[(&'static str, &str)]| retry_wait(&headers(pairs), now);
        assert_eq!(wait(&[]), Duration::from_secs(5));
        assert_eq!(wait(&[("retry-after", "12")]), Duration::from_secs(12));
        assert_eq!(wait(&[("retry-after", "3600")]), Duration::from_secs(60));
        assert_eq!(wait(&[("retry-after", "soon")]), Duration::from_secs(5));
        assert_eq!(
            wait(&[("x-ratelimit-retry-after", "1800000009")]),
            Duration::from_secs(9)
        );
        assert_eq!(
            wait(&[("x-ratelimit-retry-after", "1799999999")]),
            Duration::ZERO,
            "already past"
        );
    }

    #[tokio::test]
    async fn sends_hon_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manga"))
            .and(header("user-agent", USER_AGENT))
            .respond_with(json(SEARCH))
            .expect(1)
            .mount(&server)
            .await;
        // plain client: the source itself must set the header
        let md = MangaDex::new(reqwest::Client::new(), &server.uri());
        md.search("x").await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn live_smoke() {
        let md = MangaDex::new(crate::sources::http_client(), "https://api.mangadex.org");
        let results = md.search("solo leveling").await.unwrap();
        assert!(!results.is_empty());
    }
}
