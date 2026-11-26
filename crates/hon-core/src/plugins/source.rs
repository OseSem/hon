//! `PluginSource`: a loaded plugin as a [`Source`], with everything it returns checked and
//! normalized, since plugin output is untrusted.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use mlua::{IntoLua, Lua, LuaString, Table, Value};

use crate::model::{ChapterNo, ChapterRef, MangaRef, PageRef, SourceId};
use crate::plugins::host::LogSink;
use crate::plugins::host::http::{BODY_CAP, Http, RetryPolicy};
use crate::plugins::runtime::{HostConfig, LibSource, Manifest, Runtime, display_text};
use crate::sources::Source;

/// Results beyond this are dropped.
pub const MAX_SEARCH_RESULTS: usize = 2_000;
/// More chapters than this is an error.
pub const MAX_CHAPTERS: usize = 20_000;
/// More pages than this is an error.
pub const MAX_PAGES: usize = 2_000;
/// The longest string a plugin may return, except `fetch_page` bytes.
pub const MAX_STRING: usize = 64 << 10;
/// Alternative titles beyond this are dropped.
pub const MAX_ALT_TITLES: usize = 32;
/// The most string bytes hon copies out of one call's result. Many table slots can share
/// one Lua string, so the per-string cap alone doesn't bound the copies.
pub const TEXT_BUDGET: usize = 64 << 20;
/// Longer error lines from a plugin are cut.
const MAX_ERROR_LINE: usize = 512;
const MAX_ID: usize = 64;

/// How a plugin is run.
#[derive(Clone)]
pub struct LoadOpts {
    pub retry: RetryPolicy,
    /// Wall time for loading the plugin and for each call.
    pub deadline: Duration,
    pub log: LogSink,
}

impl Default for LoadOpts {
    fn default() -> Self {
        LoadOpts {
            retry: RetryPolicy::default(),
            deadline: Duration::from_secs(60),
            log: Arc::new(|_, _| {}),
        }
    }
}

pub struct PluginSource {
    id: SourceId,
    runtime: Runtime,
    http: Arc<Http>,
    /// The most bytes the plugin's own `fetch_page` may return.
    page_cap: usize,
}

impl PluginSource {
    /// Loads a plugin with its own `http` client and refuses one this hon can't run. The
    /// runtime refuses a newer `api` first and cleans the manifest `name` for display.
    pub fn load(
        code: &str,
        chunk_name: &str,
        libs: LibSource,
        opts: LoadOpts,
    ) -> anyhow::Result<PluginSource> {
        let http = Arc::new(Http::new(opts.retry, BODY_CAP));
        let host = HostConfig::new(opts.deadline, http.clone(), opts.log);
        let runtime =
            Runtime::load(code, chunk_name, libs, host).map_err(|e| one_line(chunk_name, &e))?;
        let m = &runtime.manifest;
        if !valid_id(&m.id) {
            bail!(
                "{chunk_name}: plugin field `id` must be 1 to {MAX_ID} lowercase letters, digits, \
                 `-` or `_`, starting with a letter or digit"
            );
        }
        for (field, value) in [("name", &m.name), ("lang", &m.lang)] {
            if value.len() > MAX_STRING {
                bail!("{chunk_name}: plugin field `{field}` is longer than 64 KiB");
            }
        }
        if m.lang.chars().any(char::is_control) {
            bail!("{chunk_name}: plugin field `lang` contains a control character");
        }
        Ok(PluginSource {
            id: SourceId::new(&m.id, &m.name),
            runtime,
            http,
            page_cap: BODY_CAP,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.runtime.manifest
    }

    fn name(&self) -> &str {
        self.id.name()
    }

    /// Calls a plugin function; Lua errors come back as one line, without mlua's prefix or the
    /// stack traceback.
    async fn call(&self, func: &str, arg: Arg) -> anyhow::Result<Value> {
        self.runtime
            .call::<_, Value>(func, arg)
            .await
            .map_err(|e| one_line(self.name(), &e))
    }

    fn reader(&self, func: &'static str) -> Reader<'_> {
        Reader {
            name: self.name(),
            func,
            budget: Cell::new(TEXT_BUDGET),
        }
    }

    fn manga(&self, r: &Reader, i: usize, entry: Value) -> anyhow::Result<MangaRef> {
        let t = r.table(i, entry)?;
        let what = |key| format!("search result #{i}: `{key}`");
        let id = r.required(&what("id"), r.get(&t, "id")?, Text::Ident)?;
        let title = r.required(&what("title"), r.get(&t, "title")?, Text::Display)?;
        let url = r.required(&what("url"), r.get(&t, "url")?, Text::Ident)?;
        let alt_what = what("alt_titles");
        let alt_titles = match r.get(&t, "alt_titles")? {
            Value::Nil => Vec::new(),
            Value::Table(list) => {
                let mut alts = Vec::new();
                for j in 1..=list.raw_len().min(MAX_ALT_TITLES) {
                    let Value::String(s) = r.index(&list, j)? else {
                        return Err(r.err(format!("{alt_what} must be a list of strings")));
                    };
                    let alt = r.text(&alt_what, &s, Text::Display)?;
                    if !alt.is_empty() {
                        alts.push(alt);
                    }
                }
                alts
            }
            _ => return Err(r.err(format!("{alt_what} must be a list of strings"))),
        };
        Ok(MangaRef {
            source: self.id.clone(),
            id,
            title,
            alt_titles,
            url,
        })
    }

    /// `None` when the chapter number doesn't parse.
    fn chapter(&self, r: &Reader, i: usize, entry: Value) -> anyhow::Result<Option<ChapterRef>> {
        let t = r.table(i, entry)?;
        let what = |key| format!("chapters result #{i}: `{key}`");
        let id = r.required(&what("id"), r.get(&t, "id")?, Text::Ident)?;
        let number = match r.get(&t, "number")? {
            Value::String(s) => r.number(&s)?,
            v @ (Value::Integer(_) | Value::Number(_)) => lua_number(&v),
            _ => {
                return Err(r.err(format!("{} must be a string or a number", what("number"))));
            }
        };
        let title = match r.get(&t, "title")? {
            Value::Nil => None,
            v => Some(r.string(&what("title"), v, Text::Display)?).filter(|t| !t.is_empty()),
        };
        let published = match r.get(&t, "published")? {
            Value::Integer(secs) => jiff::Timestamp::from_second(secs).ok(),
            _ => None,
        };
        let Some(number) = number.as_deref().and_then(ChapterNo::parse) else {
            return Ok(None);
        };
        Ok(Some(ChapterRef {
            source: self.id.clone(),
            id,
            number,
            title,
            published,
        }))
    }
}

#[async_trait::async_trait]
impl Source for PluginSource {
    fn id(&self) -> SourceId {
        self.id.clone()
    }

    async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>> {
        let list = self.call("search", Arg::Query(query.to_string())).await?;
        let r = self.reader("search");
        let entries = r.entries(list, MAX_SEARCH_RESULTS, true)?;
        entries
            .into_iter()
            .enumerate()
            .map(|(i, entry)| self.manga(&r, i + 1, entry))
            .collect()
    }

    async fn chapters(&self, manga: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
        let arg = Arg::Table(vec![
            ("id", Some(manga.id.clone())),
            ("title", Some(manga.title.clone())),
            ("url", Some(manga.url.clone())),
        ]);
        let list = self.call("chapters", arg).await?;
        let r = self.reader("chapters");
        let entries = r.entries(list, MAX_CHAPTERS, false)?;
        let mut chapters = Vec::new();
        for (i, entry) in entries.into_iter().enumerate() {
            chapters.extend(self.chapter(&r, i + 1, entry)?);
        }
        // A stable sort keeps the first of each number first, and dedup keeps the first.
        chapters.sort_by_key(|c| c.number);
        chapters.dedup_by_key(|c| c.number);
        Ok(chapters)
    }

    async fn pages(&self, chapter: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
        let arg = Arg::Table(vec![
            ("id", Some(chapter.id.clone())),
            ("number", Some(chapter.number.to_string())),
            ("title", chapter.title.clone()),
        ]);
        let list = self.call("pages", arg).await?;
        let r = self.reader("pages");
        let entries = r.entries(list, MAX_PAGES, false)?;
        entries
            .into_iter()
            .enumerate()
            .map(|(i, entry)| {
                let url =
                    r.required(&format!("pages result #{i}", i = i + 1), entry, Text::Ident)?;
                Ok(PageRef { url })
            })
            .collect()
    }

    /// The plugin's own `fetch_page` if it has one, else a GET with its headers. The GET
    /// doesn't enter Lua, so page downloads aren't serialized behind the plugin's calls.
    async fn fetch_page(&self, page: &PageRef) -> anyhow::Result<Vec<u8>> {
        if !self.runtime.manifest.has_fetch_page {
            return self
                .http
                .fetch_bytes(&page.url)
                .await
                .map_err(|e| one_line(self.name(), &e));
        }
        let arg = Arg::Table(vec![("url", Some(page.url.clone()))]);
        let Value::String(bytes) = self.call("fetch_page", arg).await? else {
            bail!("{}: `fetch_page` must return a string", self.name());
        };
        let bytes = bytes.as_bytes();
        if bytes.len() > self.page_cap {
            bail!(
                "{}: `fetch_page` returned more than {} bytes",
                self.name(),
                self.page_cap
            );
        }
        Ok(bytes.to_vec())
    }
}

/// Whether a string is shown to the user or used as an identifier.
#[derive(Clone, Copy)]
enum Text {
    /// Control characters become spaces (see [`display_text`]).
    Display,
    /// Control characters are an error.
    Ident,
}

/// Reads one call's result with raw access: the call's deadline has passed, so a
/// metamethod would fail. Every string copied out counts against [`TEXT_BUDGET`].
struct Reader<'a> {
    name: &'a str,
    func: &'static str,
    budget: Cell<usize>,
}

impl Reader<'_> {
    fn err(&self, msg: impl std::fmt::Display) -> anyhow::Error {
        anyhow!("{}: {msg}", self.name)
    }

    fn lua_err(&self, e: mlua::Error) -> anyhow::Error {
        one_line(self.name, &e.into())
    }

    fn get(&self, t: &Table, key: &str) -> anyhow::Result<Value> {
        t.raw_get(key).map_err(|e| self.lua_err(e))
    }

    fn index(&self, t: &Table, i: usize) -> anyhow::Result<Value> {
        t.raw_get(i).map_err(|e| self.lua_err(e))
    }

    /// The entries of the returned list, the first `cap` if `truncate`, else an error past it.
    fn entries(&self, list: Value, cap: usize, truncate: bool) -> anyhow::Result<Vec<Value>> {
        let func = self.func;
        let Value::Table(list) = list else {
            return Err(self.err(format!("`{func}` must return a list")));
        };
        let len = list.raw_len();
        if len > cap && !truncate {
            return Err(self.err(format!(
                "`{func}` returned {len} entries; the limit is {cap}"
            )));
        }
        (1..=len.min(cap)).map(|i| self.index(&list, i)).collect()
    }

    fn table(&self, i: usize, entry: Value) -> anyhow::Result<Table> {
        match entry {
            Value::Table(t) => Ok(t),
            _ => Err(self.err(format!("{} result #{i} must be a table", self.func))),
        }
    }

    /// Counts `len` bytes against the budget.
    fn spend(&self, len: usize) -> anyhow::Result<()> {
        let left = self.budget.get();
        if len > left {
            return Err(self.err(format!("`{}` returned more than 64 MiB of text", self.func)));
        }
        self.budget.set(left - len);
        Ok(())
    }

    /// A non-empty string; `what` names it in errors.
    fn required(&self, what: &str, v: Value, kind: Text) -> anyhow::Result<String> {
        let s = self.string(what, v, kind)?;
        if s.is_empty() {
            return Err(self.err(format!("{what} must not be empty")));
        }
        Ok(s)
    }

    fn string(&self, what: &str, v: Value, kind: Text) -> anyhow::Result<String> {
        match v {
            Value::String(s) => self.text(what, &s, kind),
            _ => Err(self.err(format!("{what} must be a string"))),
        }
    }

    /// A copy of `s`: UTF-8, at most [`MAX_STRING`] bytes, and safe to print.
    fn text(&self, what: &str, s: &LuaString, kind: Text) -> anyhow::Result<String> {
        let bytes = s.as_bytes();
        if bytes.len() > MAX_STRING {
            return Err(self.err(format!("{what} is longer than 64 KiB")));
        }
        self.spend(bytes.len())?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Err(self.err(format!("{what} must be valid UTF-8")));
        };
        match kind {
            Text::Display => Ok(display_text(text)),
            Text::Ident if text.chars().any(char::is_control) => {
                Err(self.err(format!("{what} contains a control character")))
            }
            Text::Ident => Ok(text.to_string()),
        }
    }

    /// A chapter number given as a string; `None` (the chapter is dropped) when it can't be
    /// one. Counts against the budget, since parsing reads it all.
    fn number(&self, s: &LuaString) -> anyhow::Result<Option<String>> {
        let bytes = s.as_bytes();
        if bytes.len() > MAX_STRING {
            return Ok(None);
        }
        self.spend(bytes.len())?;
        Ok(std::str::from_utf8(&bytes).ok().map(str::to_string))
    }
}

/// Stands in for a plugin that failed to load: every call fails with why.
pub struct BrokenSource {
    id: SourceId,
    reason: String,
}

impl BrokenSource {
    pub fn new(id: SourceId, reason: impl Into<String>) -> BrokenSource {
        BrokenSource {
            id,
            reason: reason.into(),
        }
    }
}

#[async_trait::async_trait]
impl Source for BrokenSource {
    fn id(&self) -> SourceId {
        self.id.clone()
    }

    async fn search(&self, _: &str) -> anyhow::Result<Vec<MangaRef>> {
        bail!("{}", self.reason)
    }

    async fn chapters(&self, _: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
        bail!("{}", self.reason)
    }

    async fn pages(&self, _: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
        bail!("{}", self.reason)
    }

    async fn fetch_page(&self, _: &PageRef) -> anyhow::Result<Vec<u8>> {
        bail!("{}", self.reason)
    }
}

/// What a plugin function is called with: the search query, or a table whose `None`
/// fields are nil.
enum Arg {
    Query(String),
    Table(Vec<(&'static str, Option<String>)>),
}

impl IntoLua for Arg {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        match self {
            Arg::Query(q) => q.into_lua(lua),
            Arg::Table(fields) => {
                let table = lua.create_table()?;
                for (key, value) in fields {
                    table.raw_set(key, value)?;
                }
                Ok(Value::Table(table))
            }
        }
    }
}

/// `"<prefix>: <line>"`: the first line of an error from the runtime, without mlua's
/// `runtime error:` or the stack traceback, safe to print and at most [`MAX_ERROR_LINE`]
/// bytes.
fn one_line(prefix: &str, e: &anyhow::Error) -> anyhow::Error {
    let full = format!("{e:#}");
    let msg = full
        .strip_prefix(prefix)
        .and_then(|m| m.strip_prefix(": "))
        .unwrap_or(&full);
    let mut line = msg.lines().next().unwrap_or_default().trim();
    while let Some(rest) = line.strip_prefix("runtime error: ") {
        line = rest;
    }
    let mut line = display_text(line);
    if line.len() > MAX_ERROR_LINE {
        let mut cut = MAX_ERROR_LINE;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
    }
    if line.is_empty() {
        line = "the plugin raised an error".to_string();
    }
    anyhow!("{prefix}: {line}")
}

/// A Lua number written the shortest way that reads back the same (`10`, `10.5`); `None`
/// for NaN and infinities.
fn lua_number(v: &Value) -> Option<String> {
    match *v {
        Value::Integer(n) => Some(n.to_string()),
        Value::Number(n) if n.is_finite() => Some(n.to_string()),
        _ => None,
    }
}

/// `^[a-z0-9][a-z0-9_-]{0,63}$`: the id becomes a file name.
fn valid_id(id: &str) -> bool {
    let lower_or_digit = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    match id.as_bytes().split_first() {
        Some((first, rest)) => {
            id.len() <= MAX_ID
                && lower_or_digit(first)
                && rest
                    .iter()
                    .all(|b| lower_or_digit(b) || *b == b'-' || *b == b'_')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const DEFAULT_FUNCTIONS: &str = r#"
        search = function(query) return {} end,
        chapters = function(manga) return {} end,
        pages = function(chapter) return {} end,
    "#;

    /// A plugin "t" named "T" with `fields`, where `BASE` is replaced by `base`.
    fn code(base: &str, fields: &str) -> String {
        format!(
            r#"local BASE = "{base}"
            return {{
                id = "t", name = "T", lang = "en", api = 1, requests_per_second = 50,
                {fields}
            }}"#
        )
    }

    fn no_libs() -> LibSource {
        LibSource::Dir(PathBuf::from("/nonexistent"))
    }

    fn fast_opts() -> LoadOpts {
        LoadOpts {
            retry: RetryPolicy {
                retries: 3,
                backoff: [Duration::from_millis(1); 3],
                max_wait: Duration::from_secs(60),
            },
            ..LoadOpts::default()
        }
    }

    fn load(base: &str, fields: &str) -> PluginSource {
        PluginSource::load(&code(base, fields), "t.lua", no_libs(), fast_opts()).unwrap()
    }

    fn load_err(code: &str) -> String {
        match PluginSource::load(code, "t.lua", no_libs(), fast_opts()) {
            Ok(_) => panic!("expected the plugin to fail to load"),
            Err(e) => e.to_string(),
        }
    }

    fn manga(source: &PluginSource, id: &str) -> MangaRef {
        MangaRef {
            source: source.id(),
            id: id.to_string(),
            title: "M".to_string(),
            alt_titles: Vec::new(),
            url: format!("https://example.com/{id}"),
        }
    }

    fn chapter(source: &PluginSource, id: &str) -> ChapterRef {
        ChapterRef {
            source: source.id(),
            id: id.to_string(),
            number: ChapterNo::parse("1").unwrap(),
            title: None,
            published: None,
        }
    }

    fn numbers(chapters: &[ChapterRef]) -> Vec<String> {
        chapters.iter().map(|c| c.number.to_string()).collect()
    }

    fn html(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/html")
            .set_body_string(body)
    }

    const SITE_PLUGIN: &str = r##"
        search = function(query)
            local doc = html.parse(http.get(BASE .. "/search", { query = { q = query } }).body)
            local out = {}
            for _, a in doc:select("#r a") do
                table.insert(out, {
                    id = a:attr("href"), title = a:text(), url = BASE .. a:attr("href"),
                    alt_titles = { a:attr("data-alt") },
                })
            end
            return out
        end,
        chapters = function(manga)
            local doc = html.parse(http.get(BASE .. manga.id).body)
            local out = {}
            for _, li in doc:select("li") do
                table.insert(out, {
                    id = li:attr("data-id"), number = li:attr("data-n"), title = li:text(),
                })
            end
            return out
        end,
        pages = function(chapter)
            local doc = html.parse(http.get(BASE .. "/chapter/" .. chapter.id).body)
            local out = {}
            for _, img in doc:select("img") do table.insert(out, img:attr("src")) end
            return out
        end,
    "##;

    #[tokio::test]
    async fn search_chapters_pages_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(path("/search"))
            .and(query_param("q", "one"))
            .respond_with(html(
                r##"<div id="r"><a href="/manga/one" data-alt="Uno">One</a></div>"##,
            ))
            .mount(&server)
            .await;
        Mock::given(path("/manga/one"))
            .respond_with(html(
                r#"<ul>
                    <li data-id="c2" data-n="2">Two</li>
                    <li data-id="c1" data-n="1">One</li>
                    <li data-id="c15" data-n="1.5"> </li>
                    <li data-id="c1b" data-n="1">One again</li>
                </ul>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(path("/chapter/c1"))
            .respond_with(html(
                r#"<img src="https://img/1.png"><img src="https://img/2.png"><img src="https://img/3.png">"#,
            ))
            .mount(&server)
            .await;
        let source = load(&server.uri(), SITE_PLUGIN);
        assert_eq!(source.id().slug(), "t");
        assert_eq!(source.id().name(), "T");

        let results = source.search("one").await.unwrap();
        assert_eq!(
            results,
            vec![MangaRef {
                source: SourceId::new("t", "T"),
                id: "/manga/one".to_string(),
                title: "One".to_string(),
                alt_titles: vec!["Uno".to_string()],
                url: format!("{}/manga/one", server.uri()),
            }]
        );
        assert_eq!(results[0].source.slug(), "t");

        let chapters = source.chapters(&results[0]).await.unwrap();
        assert_eq!(numbers(&chapters), ["1", "1.5", "2"]);
        let ids: Vec<_> = chapters.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c1", "c15", "c2"]);
        assert_eq!(chapters[0].title.as_deref(), Some("One"));
        assert_eq!(chapters[1].title, None, "a blank title is no title");
        assert!(chapters.iter().all(|c| c.source.slug() == "t"));

        let pages = source.pages(&chapters[0]).await.unwrap();
        let urls: Vec<_> = pages.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://img/1.png",
                "https://img/2.png",
                "https://img/3.png"
            ]
        );
    }

    #[tokio::test]
    async fn plugin_receives_manga_and_chapter_fields() {
        let source = load(
            "",
            r#"
            search = function(query) return {} end,
            chapters = function(manga)
                return { { id = manga.id .. "|" .. manga.title .. "|" .. manga.url, number = 1 } }
            end,
            pages = function(chapter)
                return { chapter.id .. "|" .. chapter.number .. "|" .. tostring(chapter.title) }
            end,
            "#,
        );
        let chapters = source.chapters(&manga(&source, "m")).await.unwrap();
        assert_eq!(chapters[0].id, "m|M|https://example.com/m");
        let mut ch = chapter(&source, "c");
        ch.number = ChapterNo::parse("10.5").unwrap();
        ch.title = Some("Ten".to_string());
        let pages = source.pages(&ch).await.unwrap();
        assert_eq!(pages[0].url, "c|10.5|Ten");
        ch.title = None;
        let pages = source.pages(&ch).await.unwrap();
        assert_eq!(pages[0].url, "c|10.5|nil");
    }

    #[tokio::test]
    async fn default_fetch_page_sends_manifest_headers() {
        let server = MockServer::start().await;
        let referer = format!("{}/", server.uri());
        Mock::given(method("GET"))
            .and(path("/p.png"))
            .and(header("referer", referer.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"image".to_vec()))
            .mount(&server)
            .await;
        let fields = format!(r#"headers = {{ Referer = BASE .. "/" }}, {DEFAULT_FUNCTIONS}"#);
        let source = load(&server.uri(), &fields);
        let page = PageRef {
            url: format!("{}/p.png", server.uri()),
        };
        assert_eq!(source.fetch_page(&page).await.unwrap(), b"image");

        let bare = load(&server.uri(), DEFAULT_FUNCTIONS);
        let err = bare.fetch_page(&page).await.unwrap_err().to_string();
        assert!(err.starts_with("T: "), "{err}");
        assert!(err.contains("404"), "{err}");
    }

    #[tokio::test]
    async fn custom_fetch_page_used_when_defined() {
        let fields = format!(
            r#"{DEFAULT_FUNCTIONS}
            fetch_page = function(page)
                if page.url ~= "https://img/p.png" then error("wrong url " .. page.url) end
                return "\x89PNG"
            end,"#
        );
        let source = load("", &fields);
        let page = PageRef {
            url: "https://img/p.png".to_string(),
        };
        assert_eq!(source.fetch_page(&page).await.unwrap(), b"\x89PNG");

        let source = load(
            "",
            &format!("{DEFAULT_FUNCTIONS} fetch_page = function() return 1 end,"),
        );
        let err = source.fetch_page(&page).await.unwrap_err().to_string();
        assert_eq!(err, "T: `fetch_page` must return a string");
    }

    const JUNK_PLUGIN: &str = r#"
        search = function(query)
            if query == "no url" then return { { id = "a", title = "A" } } end
            if query == "not a list" then return "nope" end
            if query == "not a table" then return { "a" } end
            if query == "bad alt" then return { { id = "a", title = "A", url = "u", alt_titles = { 1 } } } end
            return {}
        end,
        chapters = function(manga)
            if manga.id == "numbers" then
                return {
                    { id = "nan", number = 0/0 },
                    { id = "inf", number = 1e309 },
                    { id = "word", number = "Chapter 10" },
                    { id = "neg", number = -1 },
                    { id = "three", number = "3" },
                }
            elseif manga.id == "bad utf8" then
                return { { id = "a", number = "1", title = "\xff" } }
            elseif manga.id == "table title" then
                return { { id = "a", number = "1" }, { id = "b", number = "2", title = {} } }
            elseif manga.id == "no number" then
                return { { id = "a" } }
            elseif manga.id == "no id" then
                return { { number = "1" } }
            end
            return {}
        end,
        pages = function(chapter)
            if chapter.id == "table" then return { "https://img/1", {} } end
            return {}
        end,
    "#;

    #[tokio::test]
    async fn junk_output_is_dropped_or_named() {
        let source = load("", JUNK_PLUGIN);
        let chapters = source.chapters(&manga(&source, "numbers")).await.unwrap();
        assert_eq!(numbers(&chapters), ["3"]);
        assert_eq!(chapters[0].id, "three");

        let err = source
            .chapters(&manga(&source, "bad utf8"))
            .await
            .unwrap_err();
        let err = err.to_string();
        assert!(err.contains("`title`"), "{err}");
        assert!(err.starts_with("T: chapters result #1: "), "{err}");

        let err = source
            .chapters(&manga(&source, "table title"))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: chapters result #2: `title` must be a string"
        );
        let err = source
            .chapters(&manga(&source, "no number"))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: chapters result #1: `number` must be a string or a number"
        );
        let err = source.chapters(&manga(&source, "no id")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: chapters result #1: `id` must be a string"
        );

        let err = source.search("no url").await.unwrap_err().to_string();
        assert_eq!(err, "T: search result #1: `url` must be a string");
        let err = source.search("not a list").await.unwrap_err().to_string();
        assert_eq!(err, "T: `search` must return a list");
        let err = source.search("not a table").await.unwrap_err().to_string();
        assert_eq!(err, "T: search result #1 must be a table");
        let err = source.search("bad alt").await.unwrap_err().to_string();
        assert_eq!(
            err,
            "T: search result #1: `alt_titles` must be a list of strings"
        );

        let err = source.pages(&chapter(&source, "table")).await.unwrap_err();
        assert_eq!(err.to_string(), "T: pages result #2 must be a string");
    }

    #[tokio::test]
    async fn lua_numbers_and_published_are_normalized() {
        let source = load(
            "",
            r#"
            search = function() return {} end,
            pages = function() return {} end,
            chapters = function()
                return {
                    { id = "ten", number = 10, published = 1700000000 },
                    { id = "half", number = 10.5, published = "1700000000" },
                    { id = "big", number = 1e20, published = 1.5 },
                    { id = "float ten", number = 10.0 },
                }
            end,
            "#,
        );
        let chapters = source.chapters(&manga(&source, "m")).await.unwrap();
        assert_eq!(numbers(&chapters), ["10", "10.5", "100000000000000000000"]);
        assert_eq!(chapters[0].id, "ten");
        assert_eq!(
            chapters[0].published,
            Some(jiff::Timestamp::from_second(1_700_000_000).unwrap())
        );
        assert_eq!(chapters[1].published, None, "a string date is dropped");
        assert_eq!(chapters[2].published, None, "a fractional date is dropped");
    }

    #[test]
    fn lua_number_formats_shortest() {
        assert_eq!(lua_number(&Value::Integer(10)).as_deref(), Some("10"));
        assert_eq!(lua_number(&Value::Number(10.5)).as_deref(), Some("10.5"));
        assert_eq!(lua_number(&Value::Number(10.0)).as_deref(), Some("10"));
        assert_eq!(lua_number(&Value::Number(0.1)).as_deref(), Some("0.1"));
        assert_eq!(lua_number(&Value::Number(f64::NAN)), None);
        assert_eq!(lua_number(&Value::Number(f64::INFINITY)), None);
    }

    #[tokio::test]
    async fn caps_truncate_search_and_refuse_the_rest() {
        let source = load(
            "",
            r#"
            search = function()
                local out = {}
                for i = 1, 2005 do out[i] = { id = tostring(i), title = "t", url = "u" } end
                return out
            end,
            chapters = function(manga)
                local out = {}
                for i = 1, 20001 do out[i] = { id = tostring(i), number = i } end
                if manga.id == "ok" then out[20001] = nil end
                return out
            end,
            pages = function(chapter)
                if chapter.id == "long" then return { string.rep("x", 64 * 1024 + 1) } end
                local out = {}
                for i = 1, 2001 do out[i] = "u" end
                if chapter.id == "ok" then out[2001] = nil end
                return out
            end,
            "#,
        );
        let results = source.search("x").await.unwrap();
        assert_eq!(results.len(), 2000);
        assert_eq!(results[1999].id, "2000");

        let err = source.chapters(&manga(&source, "m")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: `chapters` returned 20001 entries; the limit is 20000"
        );
        let chapters = source.chapters(&manga(&source, "ok")).await.unwrap();
        assert_eq!(chapters.len(), 20000);

        let err = source.pages(&chapter(&source, "c")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: `pages` returned 2001 entries; the limit is 2000"
        );
        assert_eq!(
            source.pages(&chapter(&source, "ok")).await.unwrap().len(),
            2000
        );
        let err = source.pages(&chapter(&source, "long")).await.unwrap_err();
        assert_eq!(err.to_string(), "T: pages result #1 is longer than 64 KiB");
    }

    #[tokio::test]
    async fn custom_fetch_page_is_capped() {
        let fields = format!(
            r#"{DEFAULT_FUNCTIONS}
            fetch_page = function() return string.rep("x", 1024) end,"#
        );
        let mut source = load("", &fields);
        source.page_cap = 1023;
        let page = PageRef {
            url: "https://img/p.png".to_string(),
        };
        let err = source.fetch_page(&page).await.unwrap_err().to_string();
        assert_eq!(err, "T: `fetch_page` returned more than 1023 bytes");
    }

    #[tokio::test]
    async fn lua_errors_are_one_clean_line() {
        let source = load(
            "",
            r#"
            search = function() error("boom") end,
            chapters = function() local t = nil; return t.x end,
            pages = function() error("first\nsecond") end,
            "#,
        );
        let err = source.search("x").await.unwrap_err().to_string();
        assert_eq!(err, "T: t.lua:5: boom");
        let err = source.chapters(&manga(&source, "m")).await.unwrap_err();
        let err = err.to_string();
        assert!(err.starts_with("T: t.lua:6: "), "{err}");
        assert!(
            !err.contains('\n') && !err.contains("runtime error"),
            "{err}"
        );
        let err = source.pages(&chapter(&source, "c")).await.unwrap_err();
        assert_eq!(err.to_string(), "T: t.lua:7: first");
    }

    #[tokio::test]
    async fn rate_limit_text_comes_through_unchanged() {
        let server = MockServer::start().await;
        Mock::given(path("/search"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let source = load(
            &server.uri(),
            r#"
            search = function() return { http.get(BASE .. "/search").body } end,
            chapters = function() return {} end,
            pages = function() return {} end,
            "#,
        );
        let err = source.search("x").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: the site is rate limiting; try again in a few seconds"
        );
        let page = PageRef {
            url: format!("{}/search", server.uri()),
        };
        let err = source.fetch_page(&page).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: the site is rate limiting; try again in a few seconds"
        );
    }

    #[tokio::test]
    async fn dropped_call_leaves_plugin_usable() {
        let server = MockServer::start().await;
        Mock::given(path("/slow"))
            .respond_with(html("x").set_delay(Duration::from_secs(2)))
            .mount(&server)
            .await;
        Mock::given(path("/fast"))
            .respond_with(html("y"))
            .mount(&server)
            .await;
        let source = load(
            &server.uri(),
            r#"
            search = function(query)
                local body = http.get(BASE .. "/" .. query).body
                return { { id = body, title = body, url = body } }
            end,
            chapters = function() return {} end,
            pages = function() return {} end,
            "#,
        );
        let dropped = tokio::time::timeout(Duration::from_millis(100), source.search("slow")).await;
        assert!(dropped.is_err(), "the slow search should have timed out");
        let results = tokio::time::timeout(Duration::from_secs(5), source.search("fast"))
            .await
            .expect("the second search should not hang")
            .unwrap();
        assert_eq!(results[0].id, "y");
    }

    #[tokio::test]
    async fn broken_source_reports_reason() {
        let source = BrokenSource::new(SourceId::new("x", "X"), "bad file");
        assert_eq!(source.id().slug(), "x");
        let err = source.search("q").await.unwrap_err().to_string();
        assert!(err.contains("bad file"), "{err}");
        let m = MangaRef {
            source: source.id(),
            id: "m".to_string(),
            title: "M".to_string(),
            alt_titles: Vec::new(),
            url: "u".to_string(),
        };
        assert!(
            source
                .chapters(&m)
                .await
                .unwrap_err()
                .to_string()
                .contains("bad file")
        );
        let c = ChapterRef {
            source: source.id(),
            id: "c".to_string(),
            number: ChapterNo::parse("1").unwrap(),
            title: None,
            published: None,
        };
        assert!(
            source
                .pages(&c)
                .await
                .unwrap_err()
                .to_string()
                .contains("bad file")
        );
        let p = PageRef {
            url: "u".to_string(),
        };
        assert!(
            source
                .fetch_page(&p)
                .await
                .unwrap_err()
                .to_string()
                .contains("bad file")
        );
    }

    #[tokio::test]
    async fn shared_strings_are_bounded_by_the_text_budget() {
        let source = load(
            "",
            r#"
            search = function(query)
                local s = string.rep("x", 64 * 1024)
                local alts = table.create(1000000, s)
                local out = {}
                for i = 1, 2000 do out[i] = { id = "a", title = "A", url = "u", alt_titles = alts } end
                return out
            end,
            chapters = function()
                local s = string.rep("x", 64 * 1024)
                local out = {}
                for i = 1, 20000 do out[i] = { id = "a", number = i, title = s } end
                return out
            end,
            pages = function() return {} end,
            "#,
        );
        // Copying and checking 64 MiB is quick even in a debug build.
        let start = std::time::Instant::now();
        let err = source.search("x").await.unwrap_err().to_string();
        assert_eq!(err, "T: `search` returned more than 64 MiB of text");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        let start = std::time::Instant::now();
        let err = source.chapters(&manga(&source, "m")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: `chapters` returned more than 64 MiB of text"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn alt_titles_are_truncated() {
        let source = load(
            "",
            r#"
            search = function()
                local alts = {}
                for i = 1, 40 do alts[i] = "alt " .. i end
                return { { id = "a", title = "A", url = "u", alt_titles = alts } }
            end,
            chapters = function() return {} end,
            pages = function() return {} end,
            "#,
        );
        let results = source.search("x").await.unwrap();
        assert_eq!(results[0].alt_titles.len(), 32);
        assert_eq!(results[0].alt_titles[31], "alt 32");
    }

    #[tokio::test]
    async fn control_characters_never_reach_the_terminal() {
        let source = load(
            "",
            r#"
            search = function(query)
                if query == "esc" then
                    return { {
                        id = "a", url = "u",
                        title = "\27[31mRed\27[0m \27]52;c;aGk=\7 and\n\tmore",
                        alt_titles = { "\27[2J", "B\r\n" },
                    } }
                elseif query == "id" then
                    return { { id = "a\27b", title = "A", url = "u" } }
                elseif query == "url" then
                    return { { id = "a", title = "A", url = "https://x/\n" } }
                end
                error("bad \27[31m thing\7")
            end,
            chapters = function()
                return { { id = "a", number = 1, title = "\27[1m One \194\155" } }
            end,
            pages = function() return { "https://x/\0" } end,
            "#,
        );
        let results = source.search("esc").await.unwrap();
        assert_eq!(results[0].title, "[31mRed [0m ]52;c;aGk= and more");
        assert_eq!(results[0].alt_titles, ["[2J", "B"]);
        let chapters = source.chapters(&manga(&source, "m")).await.unwrap();
        assert_eq!(chapters[0].title.as_deref(), Some("[1m One"));

        let err = source.search("id").await.unwrap_err().to_string();
        assert_eq!(
            err,
            "T: search result #1: `id` contains a control character"
        );
        let err = source.search("url").await.unwrap_err().to_string();
        assert_eq!(
            err,
            "T: search result #1: `url` contains a control character"
        );
        let err = source.pages(&chapter(&source, "c")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: pages result #1 contains a control character"
        );
        let err = source.search("error").await.unwrap_err().to_string();
        assert!(!err.chars().any(char::is_control), "{err:?}");
        assert!(err.ends_with("bad [31m thing"), "{err:?}");
    }

    #[tokio::test]
    async fn name_is_cleaned_and_errors_stay_one_line() {
        let fields = r#"
            search = function() error("boom") end,
            chapters = function() return {} end,
            pages = function() return {} end,
        "#;
        let evil = code("", fields).replace(r#"name = "T""#, r#"name = "Evil\nName\27[0m""#);
        let source = PluginSource::load(&evil, "t.lua", no_libs(), fast_opts()).unwrap();
        assert_eq!(source.id().name(), "Evil Name [0m");
        assert_eq!(source.manifest().name, "Evil Name [0m");
        let err = source.search("x").await.unwrap_err().to_string();
        assert_eq!(err, "Evil Name [0m: t.lua:5: boom");

        let blank = evil.replace(r#"name = "Evil\nName\27[0m""#, r#"name = "\n\t""#);
        let err = load_err(&blank);
        assert_eq!(err, "t.lua: plugin field `name` must not be empty");
        let lang = code("", fields).replace(r#"lang = "en""#, r#"lang = "en\27""#);
        let err = load_err(&lang);
        assert_eq!(
            err,
            "t.lua: plugin field `lang` contains a control character"
        );
    }

    #[tokio::test]
    async fn long_error_lines_are_cut() {
        let source = load(
            "",
            r#"
            search = function() error(string.rep("é", 5 * 1024 * 1024)) end,
            chapters = function() return {} end,
            pages = function() return {} end,
            "#,
        );
        let err = source.search("x").await.unwrap_err().to_string();
        assert!(err.starts_with("T: t.lua:5: éé"), "{}", &err[..40]);
        assert!(err.len() <= "T: ".len() + 512, "{}", err.len());
    }

    #[tokio::test]
    async fn empty_required_strings_are_refused() {
        let source = load(
            "",
            r#"
            search = function(query)
                if query == "title" then return { { id = "a", title = " \n ", url = "u" } } end
                return { { id = "", title = "A", url = "u" } }
            end,
            chapters = function() return { { id = "", number = 1 } } end,
            pages = function() return { "u", "" } end,
            "#,
        );
        let err = source.search("id").await.unwrap_err().to_string();
        assert_eq!(err, "T: search result #1: `id` must not be empty");
        let err = source.search("title").await.unwrap_err().to_string();
        assert_eq!(err, "T: search result #1: `title` must not be empty");
        let err = source.chapters(&manga(&source, "m")).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "T: chapters result #1: `id` must not be empty"
        );
        let err = source.pages(&chapter(&source, "c")).await.unwrap_err();
        assert_eq!(err.to_string(), "T: pages result #2 must not be empty");
    }

    #[tokio::test]
    async fn newer_api_is_refused_before_other_manifest_errors() {
        let code = r#"return { id = "t", api = 2 }"#;
        let err = load_err(code);
        assert_eq!(
            err,
            "t.lua: needs a newer hon (plugin api 2, this hon supports 1)"
        );
    }

    #[tokio::test]
    async fn newer_api_is_refused() {
        let err = load_err(&code("", DEFAULT_FUNCTIONS).replace("api = 1", "api = 2"));
        assert_eq!(
            err,
            "t.lua: needs a newer hon (plugin api 2, this hon supports 1)"
        );
    }

    #[tokio::test]
    async fn id_must_be_a_safe_file_name() {
        let long = "a".repeat(65);
        for bad in [
            "",
            "-x",
            "_x",
            "Bad",
            "a/b",
            "a.b",
            "a b",
            "../x",
            long.as_str(),
        ] {
            let fields = DEFAULT_FUNCTIONS.to_string();
            let code = code("", &fields).replace(r#"id = "t""#, &format!(r#"id = "{bad}""#));
            let err = load_err(&code);
            assert!(
                err.starts_with("t.lua: plugin field `id` must be"),
                "{bad}: {err}"
            );
        }
        let ok = "a".repeat(64);
        for good in ["a", "0", "a-b_c9", ok.as_str()] {
            let code =
                code("", DEFAULT_FUNCTIONS).replace(r#"id = "t""#, &format!(r#"id = "{good}""#));
            let source = PluginSource::load(&code, "t.lua", no_libs(), fast_opts()).unwrap();
            assert_eq!(source.id().slug(), good);
        }
    }

    #[tokio::test]
    async fn load_errors_are_one_clean_line() {
        let err = load_err(r#"error("nope")"#);
        assert_eq!(err, "t.lua: t.lua:1: nope");
        let err = load_err("return {");
        assert!(err.starts_with("t.lua: "), "{err}");
        assert!(!err.contains('\n'), "{err}");
    }
}
