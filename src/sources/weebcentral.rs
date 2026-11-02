use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, anyhow};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

use crate::model::{ChapterNo, ChapterRef, MangaRef, PageRef, SourceId};
use crate::sources::Source;

const THROTTLE_RETRIES: u32 = 2;
const SEARCH_LIMIT: &str = "20";
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// The number at the end of a chapter label: "Chapter 12", "Punch 55.5", "Official Scans 239".
static LABEL_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+(?:\.\d+)?)\s*$").expect("valid regex"));

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("static selector is valid")
}

fn text_of(el: ElementRef) -> String {
    el.text().collect::<String>().trim().to_string()
}

/// The path segment after `/{prefix}/` in an absolute or relative href.
fn id_after<'a>(href: &'a str, prefix: &str) -> Option<&'a str> {
    let marker = format!("/{prefix}/");
    let rest = &href[href.find(&marker)? + marker.len()..];
    let id = rest.split(['/', '?', '#']).next()?;
    (!id.is_empty()).then_some(id)
}

pub struct WeebCentral {
    http: reqwest::Client,
    base: String,
    retry_delay: Duration,
}

impl WeebCentral {
    pub fn new(http: reqwest::Client, base: &str) -> WeebCentral {
        WeebCentral {
            http,
            base: base.trim_end_matches('/').to_string(),
            retry_delay: Duration::from_secs(2),
        }
    }

    #[cfg(test)]
    fn with_retry_delay(mut self, delay: Duration) -> WeebCentral {
        self.retry_delay = delay;
        self
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.http
            .get(url)
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .header(reqwest::header::REFERER, format!("{}/", self.base))
    }

    /// A throttled request (429 or an empty 200) is retried after `retry_delay`
    /// before giving up.
    async fn get_html(&self, req: reqwest::RequestBuilder, what: &str) -> anyhow::Result<String> {
        for attempt in 0..=THROTTLE_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay).await;
            }
            let resp = req
                .try_clone()
                .context("WeebCentral request cannot be retried")?
                .send()
                .await
                .with_context(|| format!("WeebCentral {what} request failed"))?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                continue;
            }
            let body = resp
                .error_for_status()
                .with_context(|| format!("WeebCentral returned an error for {what}"))?
                .text()
                .await
                .with_context(|| format!("reading WeebCentral {what} response failed"))?;
            if !body.trim().is_empty() {
                return Ok(body);
            }
        }
        Err(anyhow!(
            "WeebCentral: the site is rate limiting requests; try again in a few seconds"
        ))
    }

    fn parse_results(&self, body: &str) -> anyhow::Result<Vec<MangaRef>> {
        let doc = Html::parse_fragment(body);
        let results: Vec<MangaRef> = doc
            .select(&sel("article a[href]"))
            .filter_map(|a| {
                let id = id_after(a.value().attr("href")?, "series")?;
                let title = a
                    .select(&sel("h2"))
                    .next()
                    .map(text_of)
                    .or_else(|| a.value().attr("data-tip").map(str::to_string))
                    .filter(|t| !t.is_empty())?;
                Some(MangaRef {
                    source: SourceId::WeebCentral,
                    id: id.to_string(),
                    title,
                    alt_titles: vec![],
                    url: format!("{}/series/{id}", self.base),
                })
            })
            .collect();
        if results.is_empty() && !body.contains("No results found") {
            return Err(anyhow!("WeebCentral: could not find the search results"));
        }
        Ok(results)
    }
}

fn parse_chapters(body: &str) -> anyhow::Result<Vec<ChapterRef>> {
    let doc = Html::parse_fragment(body);
    let label = sel("span.grow > span");
    let time = sel("time[datetime]");
    let mut links = 0;
    let mut by_number: BTreeMap<ChapterNo, ChapterRef> = BTreeMap::new();
    for a in doc.select(&sel("a[href]")) {
        let Some(id) = a.value().attr("href").and_then(|h| id_after(h, "chapters")) else {
            continue;
        };
        links += 1;
        let Some(text) = a.select(&label).next().map(text_of) else {
            continue;
        };
        if text.to_lowercase().starts_with("volume") {
            continue;
        }
        let Some(number) = LABEL_NUMBER
            .captures(&text)
            .and_then(|c| ChapterNo::parse(&c[1]))
        else {
            continue;
        };
        let published = a
            .select(&time)
            .next()
            .and_then(|t| t.value().attr("datetime")?.parse().ok());
        // The list runs newest first; on a duplicate number the first row wins.
        by_number.entry(number).or_insert(ChapterRef {
            source: SourceId::WeebCentral,
            id: id.to_string(),
            number,
            title: None,
            published,
        });
    }
    if links == 0 {
        return Err(anyhow!("WeebCentral: could not find the chapter list"));
    }
    if by_number.is_empty() {
        return Err(anyhow!("WeebCentral: could not find chapter numbers"));
    }
    Ok(by_number.into_values().collect())
}

fn parse_pages(body: &str) -> anyhow::Result<Vec<PageRef>> {
    let doc = Html::parse_fragment(body);
    let pages: Vec<PageRef> = doc
        .select(&sel("#chapter-images img[src]"))
        .filter_map(|img| img.value().attr("src"))
        .filter(|src| src.starts_with("http"))
        .map(|src| PageRef {
            url: src.to_string(),
        })
        .collect();
    if pages.is_empty() {
        return Err(anyhow!("WeebCentral: could not find chapter images"));
    }
    Ok(pages)
}

#[async_trait::async_trait]
impl Source for WeebCentral {
    fn id(&self) -> SourceId {
        SourceId::WeebCentral
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>> {
        let req = self.get(&format!("{}/search/data", self.base)).query(&[
            ("text", query),
            ("limit", SEARCH_LIMIT),
            ("display_mode", "Minimal Display"),
        ]);
        let body = self.get_html(req, "search").await?;
        self.parse_results(&body)
    }

    async fn chapters(&self, manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
        let req = self.get(&format!(
            "{}/series/{}/full-chapter-list",
            self.base, manga.id
        ));
        let body = self.get_html(req, "chapter list").await?;
        parse_chapters(&body)
    }

    async fn pages(&self, chapter: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
        let req = self
            .get(&format!("{}/chapters/{}/images", self.base, chapter.id))
            .query(&[
                ("is_prev", "False"),
                ("current_page", "1"),
                ("reading_style", "long_strip"),
            ]);
        let body = self.get_html(req, "chapter images").await?;
        parse_pages(&body)
    }

    async fn fetch_page(&self, page: &PageRef) -> anyhow::Result<Vec<u8>> {
        let bytes = self
            .get(&page.url)
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
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const SEARCH: &str = include_str!("../../tests/fixtures/weebcentral/search.html");
    const SEARCH_EMPTY: &str = include_str!("../../tests/fixtures/weebcentral/search_empty.html");
    const CHAPTER_LIST: &str =
        include_str!("../../tests/fixtures/weebcentral/full_chapter_list.html");
    const IMAGES: &str = include_str!("../../tests/fixtures/weebcentral/chapter_images.html");

    const OPM: &str = "01J76XY7KT7J224EBK6J816Y1Q";

    fn html(body: impl Into<String>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.into(), "text/html; charset=utf-8")
    }

    fn manga(id: &str) -> MangaRef {
        MangaRef {
            source: SourceId::WeebCentral,
            id: id.into(),
            title: String::new(),
            alt_titles: vec![],
            url: String::new(),
        }
    }

    fn chapter(id: &str) -> ChapterRef {
        ChapterRef {
            source: SourceId::WeebCentral,
            id: id.into(),
            number: ChapterNo::parse("1").unwrap(),
            title: None,
            published: None,
        }
    }

    fn wc(server: &MockServer) -> WeebCentral {
        WeebCentral::new(reqwest::Client::new(), &server.uri()).with_retry_delay(Duration::ZERO)
    }

    #[tokio::test]
    async fn search_lists_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .and(query_param("text", "solo leveling"))
            .and(query_param("display_mode", "Minimal Display"))
            .respond_with(html(SEARCH))
            .mount(&server)
            .await;
        let results = wc(&server).search("solo leveling").await.unwrap();
        let titles: Vec<&str> = results.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Solo Leveling",
                "Solo Leveling (Volume)",
                "Solo Leveling: Ragnarok"
            ]
        );
        let solo = &results[0];
        assert_eq!(solo.id, "01J76XYCPSY3C4BNPBRY8JMCBE");
        assert_eq!(solo.source, SourceId::WeebCentral);
        assert_eq!(
            solo.url,
            format!("{}/series/01J76XYCPSY3C4BNPBRY8JMCBE", server.uri())
        );
    }

    #[tokio::test]
    async fn search_empty_list_is_ok_and_changed_html_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .and(query_param("text", "none"))
            .respond_with(html(SEARCH_EMPTY))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .and(query_param("text", "changed"))
            .respond_with(html("<div class=\"results\"><p>Solo Leveling</p></div>"))
            .mount(&server)
            .await;
        let wc = wc(&server);
        assert!(wc.search("none").await.unwrap().is_empty());
        let err = wc.search("changed").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("WeebCentral: could not find the search results"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn chapters_full_list_sorted_deduped_with_fractions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/series/{OPM}/full-chapter-list")))
            .respond_with(html(CHAPTER_LIST))
            .mount(&server)
            .await;
        let chapters = wc(&server).chapters(&manga(OPM)).await.unwrap();
        // 37 rows; "Official Scans 169–175" and "Punch 169–175" share 7 numbers
        assert_eq!(chapters.len(), 30);
        assert!(chapters.windows(2).all(|w| w[0].number < w[1].number));
        let numbers: Vec<String> = chapters.iter().map(|c| c.number.to_string()).collect();
        assert_eq!(numbers.first().map(String::as_str), Some("1"));
        assert_eq!(numbers.last().map(String::as_str), Some("242"));
        for n in ["55", "55.5", "55.6", "55.7", "56"] {
            assert!(numbers.iter().any(|x| x == n), "missing {n}: {numbers:?}");
        }
        let ch = |n: &str| chapters.iter().find(|c| c.number.to_string() == n).unwrap();
        // the newest row (listed first) wins a duplicate number
        assert_eq!(ch("175").id, "01JBVRQK3N70YXRA8NVWQXW3R6");
        assert_eq!(ch("55.5").id, "01J76XYYRDP48H09DETCFHJTHP");
        assert_eq!(ch("242").id, "01M5KHCBE9Y56XTJBCHZX46KEN");
        assert_eq!(
            ch("242").published,
            Some("2026-10-23T04:47:49.193Z".parse().unwrap())
        );
        assert!(chapters.iter().all(|c| c.published.is_some()));
        assert!(chapters.iter().all(|c| c.source == SourceId::WeebCentral));
    }

    #[tokio::test]
    async fn chapters_changed_html_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/series/a/full-chapter-list"))
            .respond_with(html("<div><a href=\"/series/a\">Chapter 1</a></div>"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/series/b/full-chapter-list"))
            .respond_with(html("<div><a href=\"/chapters/X\"><b>1</b></a></div>"))
            .mount(&server)
            .await;
        let wc = wc(&server);
        let err = wc.chapters(&manga("a")).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("WeebCentral: could not find the chapter list"),
            "{err:#}"
        );
        let err = wc.chapters(&manga("b")).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("WeebCentral: could not find chapter numbers"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn pages_in_order() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chapters/01J76XYXYZHAP5EMZ62S0G3WGA/images"))
            .and(query_param("reading_style", "long_strip"))
            .respond_with(html(IMAGES))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/chapters/changed/images"))
            .respond_with(html("<section id=\"pages\"><img src=\"/x.png\"></section>"))
            .mount(&server)
            .await;
        let wc = wc(&server);
        let pages = wc
            .pages(&chapter("01J76XYXYZHAP5EMZ62S0G3WGA"))
            .await
            .unwrap();
        let expected: Vec<String> = (1..=22)
            .map(|i| format!("https://hot.planeptune.us/manga/Solo-Leveling/0001-{i:03}.png"))
            .collect();
        let urls: Vec<String> = pages.into_iter().map(|p| p.url).collect();
        assert_eq!(urls, expected);

        let err = wc.pages(&chapter("changed")).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("WeebCentral: could not find chapter images"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn sends_referer_and_browser_ua() {
        let server = MockServer::start().await;
        let referer = format!("{}/", server.uri());
        // wiremock's `header` matcher splits values on commas, which a browser UA contains
        let browser_headers = move |req: &Request| {
            let get = |name: &str| req.headers.get(name).and_then(|v| v.to_str().ok());
            get("user-agent") == Some(BROWSER_UA) && get("referer") == Some(referer.as_str())
        };
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .and(browser_headers.clone())
            .respond_with(html(SEARCH))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/img/1.png"))
            .and(browser_headers.clone())
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
            .expect(1)
            .mount(&server)
            .await;
        // plain client: the source itself must set the headers
        let wc = WeebCentral::new(reqwest::Client::new(), &server.uri());
        wc.search("x").await.unwrap();
        let bytes = wc
            .fetch_page(&PageRef {
                url: format!("{}/img/1.png", server.uri()),
            })
            .await
            .unwrap();
        assert_eq!(bytes, [1, 2, 3]);
    }

    #[tokio::test]
    async fn throttled_responses_are_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .respond_with(html(""))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .respond_with(html(SEARCH))
            .expect(1)
            .mount(&server)
            .await;
        let results = wc(&server).search("solo leveling").await.unwrap();
        assert!(results.iter().any(|m| m.title == "Solo Leveling"));
    }

    #[tokio::test]
    async fn persistent_throttling_is_rate_limit_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/data"))
            .respond_with(html(" \n "))
            .expect(3)
            .mount(&server)
            .await;
        let err = wc(&server).search("solo leveling").await.unwrap_err();
        assert!(
            format!("{err:#}").contains(
                "WeebCentral: the site is rate limiting requests; try again in a few seconds"
            ),
            "{err:#}"
        );
        server.verify().await;
    }

    #[tokio::test]
    #[ignore]
    async fn live_smoke() {
        let wc = WeebCentral::new(crate::sources::http_client(), "https://weebcentral.com");
        let results = wc.search("solo leveling").await.unwrap();
        let solo = results
            .iter()
            .find(|m| m.title == "Solo Leveling")
            .expect("Solo Leveling in results");
        let chapters = wc.chapters(solo).await.unwrap();
        assert!(chapters.len() > 150, "{} chapters", chapters.len());
        tokio::time::sleep(Duration::from_secs(1)).await;
        let first = chapters
            .iter()
            .find(|c| c.number.to_string() == "1")
            .unwrap();
        let pages = wc.pages(first).await.unwrap();
        assert!(pages.len() > 5, "{} pages", pages.len());
        tokio::time::sleep(Duration::from_secs(1)).await;
        let bytes = wc.fetch_page(&pages[0]).await.unwrap();
        assert!(bytes.len() > 1000);
    }
}
