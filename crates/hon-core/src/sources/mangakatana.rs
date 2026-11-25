use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, anyhow};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

use crate::model::{ChapterNo, ChapterRef, MangaRef, PageRef, SourceId};
use crate::sources::Source;

const EMPTY_RETRIES: u32 = 2;
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

static CHAPTER_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter\s+([\d.]+)").expect("valid regex"));
static CHAPTER_HREF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/c([\d.]+)$").expect("valid regex"));
static ARRAY_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-src['"],\s*(\w+)"#).expect("valid regex"));
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"['"]([^'"]+)['"]"#).expect("valid regex"));

fn sel(css: &str) -> Selector {
    Selector::parse(css).expect("static selector is valid")
}

fn text_of(el: ElementRef) -> String {
    el.text().collect::<String>().trim().to_string()
}

pub struct MangaKatana {
    http: reqwest::Client,
    base: String,
    retry_delay: Duration,
}

impl MangaKatana {
    pub fn new(http: reqwest::Client, base: &str) -> MangaKatana {
        MangaKatana {
            http,
            base: base.trim_end_matches('/').to_string(),
            retry_delay: Duration::from_secs(2),
        }
    }

    #[cfg(test)]
    fn with_retry_delay(mut self, delay: Duration) -> MangaKatana {
        self.retry_delay = delay;
        self
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.http
            .get(url)
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .header(reqwest::header::REFERER, format!("{}/", self.base))
    }

    /// MangaKatana answers rapid repeat requests with an empty 200, so an empty
    /// body is retried after `retry_delay` before giving up.
    async fn get_html(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> anyhow::Result<(String, String)> {
        for attempt in 0..=EMPTY_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay).await;
            }
            let resp = req
                .try_clone()
                .context("MangaKatana request cannot be retried")?
                .send()
                .await
                .with_context(|| format!("MangaKatana {what} request failed"))?
                .error_for_status()
                .with_context(|| format!("MangaKatana returned an error for {what}"))?;
            let path = resp.url().path().to_string();
            let body = resp
                .text()
                .await
                .with_context(|| format!("reading MangaKatana {what} response failed"))?;
            if !body.trim().is_empty() {
                return Ok((path, body));
            }
        }
        Err(anyhow!(
            "MangaKatana: the site returned an empty page (rate limited); try again in a few seconds"
        ))
    }

    /// Path of an href (absolute or relative) on this site.
    fn path_of(&self, href: &str) -> Option<String> {
        let base = reqwest::Url::parse(&self.base).ok()?;
        Some(base.join(href).ok()?.path().to_string())
    }

    fn manga_id(path: &str) -> Option<&str> {
        let id = path.strip_prefix("/manga/")?.split('/').next()?;
        (!id.is_empty()).then_some(id)
    }

    fn manga_ref(&self, id: &str, title: String) -> MangaRef {
        MangaRef {
            source: SourceId::MangaKatana,
            id: id.to_string(),
            title,
            alt_titles: vec![],
            url: format!("{}/manga/{id}", self.base),
        }
    }

    fn parse_results(&self, body: &str) -> anyhow::Result<Vec<MangaRef>> {
        let doc = Html::parse_document(body);
        // An empty result list still renders an (empty) #book_list.
        if doc.select(&sel("#book_list")).next().is_none() {
            return Err(anyhow!(
                "MangaKatana: could not find the search result list"
            ));
        }
        Ok(doc
            .select(&sel("#book_list .item h3.title a"))
            .filter_map(|a| {
                let path = self.path_of(a.value().attr("href")?)?;
                let id = Self::manga_id(&path)?;
                let title = text_of(a);
                (!title.is_empty()).then(|| self.manga_ref(id, title))
            })
            .collect())
    }

    fn parse_manga_page(&self, path: &str, body: &str) -> anyhow::Result<MangaRef> {
        let id = Self::manga_id(path)
            .with_context(|| format!("MangaKatana: unexpected manga page path {path}"))?;
        let doc = Html::parse_document(body);
        let title = doc
            .select(&sel("h1.heading"))
            .next()
            .map(text_of)
            .filter(|t| !t.is_empty())
            .context("MangaKatana: could not find the manga title")?;
        Ok(self.manga_ref(id, title))
    }

    fn parse_chapters(&self, body: &str) -> anyhow::Result<Vec<ChapterRef>> {
        let doc = Html::parse_document(body);
        if doc.select(&sel("div.chapters")).next().is_none() {
            return Err(anyhow!("MangaKatana: could not find the chapter list"));
        }
        let link = sel("div.chapter a");
        let date = sel("div.update_time");
        let mut by_number: BTreeMap<ChapterNo, ChapterRef> = BTreeMap::new();
        for row in doc.select(&sel("div.chapters table tr")) {
            let Some(a) = row.select(&link).next() else {
                continue;
            };
            let Some(href) = a.value().attr("href") else {
                continue;
            };
            let Some(path) = self.path_of(href) else {
                continue;
            };
            let text = text_of(a);
            let number = [&*CHAPTER_TEXT, &*CHAPTER_HREF]
                .into_iter()
                .zip([&text, &path])
                .find_map(|(re, hay)| ChapterNo::parse(&re.captures(hay)?[1]));
            let Some(number) = number else { continue };
            let title = text
                .split_once(':')
                .map(|(_, t)| t.trim().to_string())
                .filter(|t| !t.is_empty());
            let published = row
                .select(&date)
                .next()
                .and_then(|d| parse_date(&text_of(d)));
            by_number.entry(number).or_insert(ChapterRef {
                source: SourceId::MangaKatana,
                id: path,
                number,
                title,
                published,
            });
        }
        Ok(by_number.into_values().collect())
    }
}

fn parse_date(s: &str) -> Option<jiff::Timestamp> {
    let date = jiff::civil::Date::strptime("%b-%d-%Y", s).ok()?;
    Some(date.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp())
}

fn parse_pages(body: &str) -> anyhow::Result<Vec<PageRef>> {
    let name = ARRAY_NAME
        .captures(body)
        .map(|c| c[1].to_string())
        .context("MangaKatana: could not find chapter images (image array name)")?;
    let array = Regex::new(&format!(
        r"(?s)var\s+{}\s*=\s*\[(.*?)\]",
        regex::escape(&name)
    ))
    .context("MangaKatana: could not build image array pattern")?
    .captures(body)
    .map(|c| c[1].to_string())
    .with_context(|| format!("MangaKatana: could not find chapter images (array {name})"))?;
    let pages: Vec<PageRef> = QUOTED
        .captures_iter(&array)
        .map(|c| PageRef {
            url: c[1].to_string(),
        })
        .collect();
    if pages.is_empty() {
        return Err(anyhow!(
            "MangaKatana: could not find chapter images (array {name} is empty)"
        ));
    }
    Ok(pages)
}

#[async_trait::async_trait]
impl Source for MangaKatana {
    fn id(&self) -> SourceId {
        SourceId::MangaKatana
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>> {
        let req = self
            .get(&format!("{}/", self.base))
            .query(&[("search", query), ("search_by", "book_name")]);
        let (path, body) = self.get_html(req, "search").await?;
        if path.starts_with("/manga/") {
            return Ok(vec![self.parse_manga_page(&path, &body)?]);
        }
        self.parse_results(&body)
    }

    async fn chapters(&self, manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
        let req = self.get(&format!("{}/manga/{}", self.base, manga.id));
        let (_, body) = self.get_html(req, "manga page").await?;
        self.parse_chapters(&body)
    }

    async fn pages(&self, chapter: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
        let req = self.get(&format!("{}{}", self.base, chapter.id));
        let (_, body) = self.get_html(req, "chapter page").await?;
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

    const SEARCH: &str = include_str!("../../tests/fixtures/mangakatana/search.html");
    const SEARCH_REDIRECT: &str =
        include_str!("../../tests/fixtures/mangakatana/search_redirect.html");
    const SEARCH_EMPTY: &str = include_str!("../../tests/fixtures/mangakatana/search_empty.html");
    const MANGA: &str = include_str!("../../tests/fixtures/mangakatana/manga.html");
    const CHAPTER: &str = include_str!("../../tests/fixtures/mangakatana/chapter.html");

    fn html(body: impl Into<String>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.into(), "text/html; charset=utf-8")
    }

    fn manga() -> MangaRef {
        MangaRef {
            source: SourceId::MangaKatana,
            id: "solo-leveling.21708".into(),
            title: "Solo Leveling".into(),
            alt_titles: vec![],
            url: String::new(),
        }
    }

    async fn serve_manga(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/manga/solo-leveling.21708"))
            .respond_with(html(MANGA))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn search_lists_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("search", "solo leveling"))
            .and(query_param("search_by", "book_name"))
            .respond_with(html(SEARCH))
            .mount(&server)
            .await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri());
        let results = mk.search("solo leveling").await.unwrap();
        let solo = results
            .iter()
            .find(|m| m.id == "solo-leveling.21708")
            .expect("Solo Leveling in results");
        assert_eq!(solo.title, "Solo Leveling");
        assert_eq!(solo.source, SourceId::MangaKatana);
        assert_eq!(
            solo.url,
            format!("{}/manga/solo-leveling.21708", server.uri())
        );
        assert!(results.iter().any(
            |m| m.title == "Solo Leveling: Ragnarok" && m.id == "solo-leveling-ragnarok.27366"
        ));
    }

    #[tokio::test]
    async fn search_redirect_to_single_manga() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/manga/solo-leveling.21708"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/manga/solo-leveling.21708"))
            .respond_with(html(SEARCH_REDIRECT))
            .mount(&server)
            .await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri());
        let results = mk.search("solo leveling").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Solo Leveling");
        assert_eq!(results[0].id, "solo-leveling.21708");
    }

    #[tokio::test]
    async fn search_empty_list_is_ok_and_changed_html_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("search", "none"))
            .respond_with(html(SEARCH_EMPTY))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("search", "changed"))
            .respond_with(html("<html><body><div id=\"results\"></div></body></html>"))
            .mount(&server)
            .await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri());
        assert!(mk.search("none").await.unwrap().is_empty());
        let err = mk.search("changed").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("MangaKatana: could not find the search result list"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn chapters_changed_html_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manga/solo-leveling.21708"))
            .respond_with(html(
                "<html><body><table><tr><td>x</td></tr></table></body></html>",
            ))
            .mount(&server)
            .await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri());
        let err = mk.chapters(&manga()).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("MangaKatana: could not find the chapter list"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn chapters_parse_sorted_ascending() {
        let server = MockServer::start().await;
        serve_manga(&server).await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri());
        let chapters = mk.chapters(&manga()).await.unwrap();
        assert!(chapters.len() > 100);
        assert!(chapters.first().unwrap().number < chapters.last().unwrap().number);
        assert!(chapters.windows(2).all(|w| w[0].number < w[1].number));
        let last = chapters.last().unwrap();
        assert_eq!(last.number.to_string(), "200");
        assert_eq!(last.title.as_deref(), Some("Side Story 21 [END]"));
        assert_eq!(last.id, "/manga/solo-leveling.21708/c200");
        assert_eq!(
            last.published,
            Some("2023-05-31T00:00:00Z".parse().unwrap())
        );
        assert!(chapters.iter().all(|c| c.published.is_some()));
        assert!(chapters.iter().all(|c| c.source == SourceId::MangaKatana));
        // the "Sequel" box on the page links to another series' chapter; it must not leak in
        assert!(
            chapters
                .iter()
                .all(|c| c.id.contains("solo-leveling.21708"))
        );
    }

    #[tokio::test]
    async fn pages_use_array_named_by_data_src() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manga/solo-leveling.21708/c112"))
            .respond_with(html(CHAPTER))
            .mount(&server)
            .await;
        let decoy = r##"<img data-src="#"/><script>
            var ytaw=['https://img/decoy.jpg',];
            var thzq=['https://img/1.jpg','https://img/2.jpg'];
            obj.attr('data-src', thzq[i]);</script>"##;
        Mock::given(method("GET"))
            .and(path("/manga/x/c1"))
            .respond_with(html(decoy))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/manga/x/c2"))
            .respond_with(html("<html>no scripts here</html>"))
            .mount(&server)
            .await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri());
        let chapter = |id: &str| ChapterRef {
            source: SourceId::MangaKatana,
            id: id.into(),
            number: ChapterNo::parse("1").unwrap(),
            title: None,
            published: None,
        };

        let pages = mk
            .pages(&chapter("/manga/solo-leveling.21708/c112"))
            .await
            .unwrap();
        let thzq = CHAPTER.split("var thzq=[").nth(1).unwrap();
        let thzq = &thzq[..thzq.find(']').unwrap()];
        assert_eq!(pages.len(), thzq.matches("https://").count());
        assert!(pages.len() > 1);

        let pages = mk.pages(&chapter("/manga/x/c1")).await.unwrap();
        let urls: Vec<&str> = pages.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["https://img/1.jpg", "https://img/2.jpg"]);

        let err = mk.pages(&chapter("/manga/x/c2")).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("MangaKatana: could not find chapter images"),
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
            .and(path("/"))
            .and(browser_headers.clone())
            .respond_with(html(SEARCH))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/img/1.jpg"))
            .and(browser_headers.clone())
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
            .expect(1)
            .mount(&server)
            .await;
        // plain client: the source itself must set the headers
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri());
        mk.search("x").await.unwrap();
        let bytes = mk
            .fetch_page(&PageRef {
                url: format!("{}/img/1.jpg", server.uri()),
            })
            .await
            .unwrap();
        assert_eq!(bytes, [1, 2, 3]);
    }

    #[tokio::test]
    async fn empty_response_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(""))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(SEARCH))
            .expect(1)
            .mount(&server)
            .await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri())
            .with_retry_delay(Duration::ZERO);
        let results = mk.search("solo leveling").await.unwrap();
        assert!(results.iter().any(|m| m.title == "Solo Leveling"));
    }

    #[tokio::test]
    async fn persistent_empty_response_is_rate_limit_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(" \n "))
            .expect(3)
            .mount(&server)
            .await;
        let mk = MangaKatana::new(reqwest::Client::new(), &server.uri())
            .with_retry_delay(Duration::ZERO);
        let err = mk.search("solo leveling").await.unwrap_err();
        assert!(
            format!("{err:#}").contains(
                "MangaKatana: the site returned an empty page (rate limited); \
                 try again in a few seconds"
            ),
            "{err:#}"
        );
        server.verify().await;
    }

    #[tokio::test]
    #[ignore]
    async fn live_smoke() {
        let mk = MangaKatana::new(crate::sources::http_client(), "https://mangakatana.com");
        let results = mk.search("solo leveling").await.unwrap();
        assert!(!results.is_empty());
    }
}
