//! `http`: the only way a plugin reaches the network. Per plugin: its own cookie jar, the
//! manifest's headers, a rate limit, retries on throttling, and a cap on response bodies.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use mlua::{Lua, Table, Value};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use tokio::time::Instant;

use crate::plugins::runtime::Manifest;

/// HTTP response bodies larger than this are refused.
pub const BODY_CAP: usize = 32 << 20;
const DEFAULT_REQUESTS_PER_SECOND: f64 = 4.0;
const RATE_LIMITED: &str = "the site is rate limiting; try again in a few seconds";

/// When a throttled request is tried again.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub retries: u32,
    /// The wait before each retry, unless the site says how long; the last one repeats.
    pub backoff: [Duration; 3],
    /// The longest wait a site's `Retry-After` can ask for.
    pub max_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            retries: 3,
            backoff: [2, 4, 8].map(Duration::from_secs),
            max_wait: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RequestOpts {
    /// Sent after the manifest headers, replacing any with the same name.
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub form: Option<Vec<(String, String)>>,
    pub body: Option<Vec<u8>>,
    /// Retry a 2xx whose body is empty or whitespace, which some sites send when throttling.
    pub retry_empty: bool,
}

impl Default for RequestOpts {
    fn default() -> Self {
        RequestOpts {
            headers: Vec::new(),
            query: Vec::new(),
            form: None,
            body: None,
            retry_empty: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    /// After redirects.
    pub url: String,
    /// Lower-cased names, one entry per name; repeated headers are joined with ", ".
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// What the manifest sets; defaults until `configure` runs.
#[derive(Debug)]
struct Settings {
    name: String,
    headers: HeaderMap,
    interval: Duration,
}

/// One plugin's HTTP client. Give each plugin its own: `configure` sets it up for one manifest.
#[derive(Debug)]
pub struct Http {
    client: reqwest::Client,
    settings: Mutex<Settings>,
    /// When the next request may be sent.
    next_send: tokio::sync::Mutex<Instant>,
    retry: RetryPolicy,
    body_cap: usize,
}

impl Default for Http {
    fn default() -> Self {
        Http::new(RetryPolicy::default(), BODY_CAP)
    }
}

impl Http {
    pub fn new(retry: RetryPolicy, body_cap: usize) -> Http {
        let client = crate::sources::http_client_builder()
            .cookie_store(true)
            .build()
            .expect("static reqwest client config is valid");
        Http {
            client,
            settings: Mutex::new(Settings {
                name: "plugin".to_string(),
                headers: HeaderMap::new(),
                interval: interval(DEFAULT_REQUESTS_PER_SECOND),
            }),
            next_send: tokio::sync::Mutex::new(Instant::now()),
            retry,
            body_cap,
        }
    }

    /// Takes the plugin's name (for errors), headers and rate limit from its manifest.
    pub fn configure(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let mut headers = HeaderMap::new();
        for (name, value) in &manifest.headers {
            let (name, value) =
                header(name, value).context("plugin field `headers` has an invalid header")?;
            headers.insert(name, value);
        }
        let mut settings = self.settings();
        settings.name = manifest.name.clone();
        settings.headers = headers;
        settings.interval = interval(manifest.requests_per_second);
        Ok(())
    }

    /// Sends a request with the manifest headers, retrying while the site throttles. Any
    /// status that isn't throttling is returned, not an error.
    pub async fn request(
        &self,
        method: Method,
        url: &str,
        opts: RequestOpts,
    ) -> anyhow::Result<Response> {
        self.send(method, url, &opts)
            .await
            .map_err(|e| self.named(e))
    }

    /// A GET whose status must be 2xx; the body.
    pub async fn fetch_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .request(Method::GET, url, RequestOpts::default())
            .await?;
        if !(200..300).contains(&resp.status) {
            return Err(self.named(anyhow!("{url} returned HTTP {}", resp.status)));
        }
        Ok(resp.body)
    }

    fn settings(&self) -> std::sync::MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn named(&self, e: anyhow::Error) -> anyhow::Error {
        anyhow!("{}: {e:#}", self.settings().name)
    }

    /// `request` without the plugin's name on errors: a Lua error gets it from the runtime.
    async fn send(
        &self,
        method: Method,
        url: &str,
        opts: &RequestOpts,
    ) -> anyhow::Result<Response> {
        let headers = self.headers_with(&opts.headers)?;
        for attempt in 0..=self.retry.retries {
            self.wait_turn().await;
            let mut req = self
                .client
                .request(method.clone(), url)
                .headers(headers.clone());
            if !opts.query.is_empty() {
                req = req.query(&opts.query);
            }
            if let Some(form) = &opts.form {
                req = req.form(form);
            }
            if let Some(body) = &opts.body {
                req = req.body(body.clone());
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("request to {url} failed"))?;
            let status = resp.status();
            let asked = retry_after(resp.headers(), jiff::Timestamp::now().as_second());
            let throttled = status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::SERVICE_UNAVAILABLE;
            if !throttled {
                let resp = self.read(resp, url).await?;
                let empty = resp.body.iter().all(u8::is_ascii_whitespace);
                if !(status.is_success() && empty && opts.retry_empty) {
                    return Ok(resp);
                }
            }
            if attempt < self.retry.retries {
                let backoff = self.retry.backoff[(attempt as usize).min(2)];
                let wait = asked.map_or(backoff, |d| d.min(self.retry.max_wait));
                tokio::time::sleep(wait).await;
            }
        }
        bail!(RATE_LIMITED)
    }

    /// The manifest headers, then `extra`, which replaces any with the same name.
    fn headers_with(&self, extra: &[(String, String)]) -> anyhow::Result<HeaderMap> {
        let mut headers = self.settings().headers.clone();
        for (name, value) in extra {
            let (name, value) = header(name, value).context("invalid request header")?;
            headers.insert(name, value);
        }
        Ok(headers)
    }

    /// Waits until the rate limit allows the next request.
    async fn wait_turn(&self) {
        let interval = self.settings().interval;
        let mut next = self.next_send.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = Instant::now() + interval;
    }

    async fn read(&self, mut resp: reqwest::Response, url: &str) -> anyhow::Result<Response> {
        let too_big = || {
            anyhow!(
                "{url}: response body is larger than {}",
                size(self.body_cap)
            )
        };
        if resp
            .content_length()
            .is_some_and(|n| n > self.body_cap as u64)
        {
            return Err(too_big());
        }
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        let headers = resp
            .headers()
            .keys()
            .map(|name| {
                let values: Vec<_> = resp
                    .headers()
                    .get_all(name)
                    .iter()
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                    .collect();
                (name.as_str().to_string(), values.join(", "))
            })
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .with_context(|| format!("reading the response from {url} failed"))?
        {
            if body.len() + chunk.len() > self.body_cap {
                return Err(too_big());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Response {
            status,
            url: final_url,
            headers,
            body,
        })
    }
}

fn size(bytes: usize) -> String {
    if bytes >= 1 << 20 && bytes.is_multiple_of(1 << 20) {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{bytes} bytes")
    }
}

fn interval(requests_per_second: f64) -> Duration {
    Duration::from_secs_f64(1.0 / requests_per_second)
}

fn header(name: &str, value: &str) -> anyhow::Result<(HeaderName, HeaderValue)> {
    let parsed_name =
        HeaderName::from_bytes(name.as_bytes()).with_context(|| format!("`{name}`"))?;
    let parsed_value = HeaderValue::from_str(value).with_context(|| format!("`{name}`"))?;
    Ok((parsed_name, parsed_value))
}

/// How long a throttled response asks to wait: `Retry-After` seconds, else
/// `X-RateLimit-Retry-After` as a Unix time (MangaDex sends that). Uncapped.
pub fn retry_after(headers: &HeaderMap, now: i64) -> Option<Duration> {
    let number =
        |name: &str| -> Option<i64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    let secs = number("retry-after").or_else(|| Some(number("x-ratelimit-retry-after")? - now))?;
    Some(Duration::from_secs(secs.max(0) as u64))
}

/// Registers the `http` global: `http.get(url, opts?)` and `http.post(url, opts?)`.
pub fn install(lua: &Lua, http: Arc<Http>) -> mlua::Result<()> {
    let module = lua.create_table()?;
    for (name, method) in [("get", Method::GET), ("post", Method::POST)] {
        let http = http.clone();
        let func =
            lua.create_async_function(move |lua, (url, opts): (String, Option<Table>)| {
                let http = http.clone();
                let method = method.clone();
                async move {
                    let opts = lua_opts(opts)?;
                    let resp = http
                        .send(method, &url, &opts)
                        .await
                        .map_err(|e| mlua::Error::external(format!("{e:#}")))?;
                    lua_response(&lua, resp)
                }
            })?;
        module.raw_set(name, func)?;
    }
    lua.globals().set("http", module)
}

/// Reads `opts` with raw access, so the plugin's metamethods don't run.
fn lua_opts(opts: Option<Table>) -> mlua::Result<RequestOpts> {
    let Some(opts) = opts else {
        return Ok(RequestOpts::default());
    };
    let form = match opts.raw_get::<Value>("form")? {
        Value::Nil => None,
        _ => Some(string_pairs(&opts, "form")?),
    };
    let body = match opts.raw_get::<Value>("body")? {
        Value::Nil => None,
        Value::String(s) => Some(s.as_bytes().to_vec()),
        _ => return Err(mlua::Error::runtime("`opts.body` must be a string")),
    };
    let retry_empty = match opts.raw_get::<Value>("retry_empty")? {
        Value::Nil => true,
        Value::Boolean(b) => b,
        _ => return Err(mlua::Error::runtime("`opts.retry_empty` must be a boolean")),
    };
    Ok(RequestOpts {
        headers: string_pairs(&opts, "headers")?,
        query: string_pairs(&opts, "query")?,
        form,
        body,
        retry_empty,
    })
}

/// `opts[key]` as name/value pairs sorted by name; values may be strings or numbers.
fn string_pairs(opts: &Table, key: &str) -> mlua::Result<Vec<(String, String)>> {
    let table = match opts.raw_get::<Value>(key)? {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(t) => t,
        _ => {
            return Err(mlua::Error::runtime(format!(
                "`opts.{key}` must be a table"
            )));
        }
    };
    let bad = || mlua::Error::runtime(format!("`opts.{key}` must map strings to strings"));
    let mut pairs = Vec::new();
    table.for_each(|k: Value, v: Value| {
        let Value::String(k) = k else {
            return Err(bad());
        };
        let v = match v {
            Value::String(s) => s.to_str()?.to_string(),
            Value::Integer(n) => n.to_string(),
            Value::Number(n) => n.to_string(),
            _ => return Err(bad()),
        };
        pairs.push((k.to_str()?.to_string(), v));
        Ok(())
    })?;
    pairs.sort();
    Ok(pairs)
}

fn lua_response(lua: &Lua, resp: Response) -> mlua::Result<Table> {
    let headers = lua.create_table()?;
    for (name, value) in resp.headers {
        headers.raw_set(name, value)?;
    }
    let table = lua.create_table()?;
    table.raw_set("status", resp.status)?;
    table.raw_set("url", resp.url)?;
    table.raw_set("headers", headers)?;
    table.raw_set("body", lua.create_string(&resp.body)?)?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use wiremock::matchers::{body_string, header as has_header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::plugins::runtime::{HostConfig, LibSource, Runtime};

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            retries: 3,
            backoff: [Duration::from_millis(1); 3],
            max_wait: Duration::from_secs(60),
        }
    }

    /// A plugin named "T" whose extra fields use `http`, with fast retries and `body_cap`.
    fn runtime_with(rps: f64, body_cap: usize, extra: &str) -> Runtime {
        let code = format!(
            r#"return {{
                id = "t", name = "T", lang = "en", api = 1, requests_per_second = {rps},
                search = function() return {{}} end,
                chapters = function() return {{}} end,
                pages = function() return {{}} end,
                {extra}
            }}"#
        );
        let mut host = HostConfig::default();
        host.http = Arc::new(Http::new(fast_retry(), body_cap));
        let libs = LibSource::Dir(PathBuf::from("/nonexistent"));
        Runtime::load(&code, "t.lua", libs, host).unwrap()
    }

    fn runtime(extra: &str) -> Runtime {
        runtime_with(50.0, BODY_CAP, extra)
    }

    /// `get(url)` returns status, final url and body of `http.get(url, <opts>)`.
    fn getter(opts: &str) -> String {
        format!(
            r#"get = function(url)
                   local r = http.get(url, {opts})
                   return r.status, r.url, r.body
               end,"#
        )
    }

    async fn get(rt: &Runtime, url: String) -> anyhow::Result<(u16, String, String)> {
        rt.call::<_, (u16, String, String)>("get", url).await
    }

    #[tokio::test]
    async fn sends_manifest_and_request_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/h"))
            .and(has_header("referer", "https://example.com/"))
            .and(has_header("x-test", "opts"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;
        let rt = runtime(&format!(
            r#"headers = {{ Referer = "https://example.com/", ["X-Test"] = "manifest" }},
               {}"#,
            getter(r#"{ headers = { ["X-Test"] = "opts" } }"#)
        ));
        let (status, _, body) = get(&rt, format!("{}/h", server.uri())).await.unwrap();
        assert_eq!((status, body.as_str()), (200, "ok"));
        let sent = &server.received_requests().await.unwrap()[0];
        assert_eq!(sent.headers.get_all("x-test").iter().count(), 1);
    }

    #[tokio::test]
    async fn returns_final_url_after_redirect() {
        let server = MockServer::start().await;
        Mock::given(path("/search"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/manga/x"))
            .mount(&server)
            .await;
        Mock::given(path("/manga/x"))
            .respond_with(ResponseTemplate::new(200).set_body_string("manga"))
            .mount(&server)
            .await;
        let rt = runtime(&getter("nil"));
        let (status, url, body) = get(&rt, format!("{}/search", server.uri())).await.unwrap();
        assert_eq!((status, body.as_str()), (200, "manga"));
        assert!(url.ends_with("/manga/x"), "{url}");
    }

    #[tokio::test]
    async fn non_2xx_is_returned_not_thrown() {
        let server = MockServer::start().await;
        Mock::given(path("/gone"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        let rt = runtime(&getter("nil"));
        let (status, _, body) = get(&rt, format!("{}/gone", server.uri())).await.unwrap();
        assert_eq!((status, body.as_str()), (404, "nope"));
    }

    #[tokio::test]
    async fn network_failure_raises() {
        let rt = runtime(&getter("nil"));
        let err = get(&rt, "http://127.0.0.1:1/x".to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("T: "), "{err}");
        assert!(err.to_string().contains("request to"), "{err}");
    }

    #[tokio::test]
    async fn posts_form_and_query() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(query_param("page", "2"))
            .and(query_param("q", "one piece"))
            .and(has_header(
                "content-type",
                "application/x-www-form-urlencoded",
            ))
            .and(body_string("text=one+piece&type=all"))
            .respond_with(ResponseTemplate::new(200).set_body_string("found"))
            .expect(1)
            .mount(&server)
            .await;
        let rt = runtime(
            r#"post = function(url)
                   local r = http.post(url, {
                       query = { q = "one piece", page = 2 },
                       form = { text = "one piece", type = "all" },
                   })
                   return r.status, r.url, r.body
               end,"#,
        );
        let (status, _, body) = rt
            .call::<_, (u16, String, String)>("post", format!("{}/search", server.uri()))
            .await
            .unwrap();
        assert_eq!((status, body.as_str()), (200, "found"));
    }

    #[tokio::test]
    async fn returns_lowercased_joined_headers_and_binary_body() {
        let server = MockServer::start().await;
        Mock::given(path("/bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("X-Many", "a")
                    .append_header("X-Many", "b")
                    .set_body_bytes(vec![0u8, 255, 1]),
            )
            .mount(&server)
            .await;
        let rt = runtime(
            r#"bin = function(url)
                   local r = http.get(url)
                   return r.headers["x-many"], #r.body, string.byte(r.body, 2)
               end,"#,
        );
        let (many, len, second) = rt
            .call::<_, (String, i64, i64)>("bin", format!("{}/bin", server.uri()))
            .await
            .unwrap();
        assert_eq!((many.as_str(), len, second), ("a, b", 3, 255));
    }

    #[tokio::test]
    async fn cookies_persist_between_calls() {
        let server = MockServer::start().await;
        Mock::given(path("/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Set-Cookie", "sid=abc; Path=/")
                    .set_body_string("in"),
            )
            .mount(&server)
            .await;
        Mock::given(path("/me"))
            .and(has_header("cookie", "sid=abc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;
        let rt = runtime(&getter("nil"));
        get(&rt, format!("{}/login", server.uri())).await.unwrap();
        let (status, _, body) = get(&rt, format!("{}/me", server.uri())).await.unwrap();
        assert_eq!((status, body.as_str()), (200, "hello"));
    }

    /// `/{name}` answers `first` twice, then 200 "ok".
    async fn flaky(server: &MockServer, name: &str, first: ResponseTemplate) {
        Mock::given(path(format!("/{name}")))
            .respond_with(first)
            .up_to_n_times(2)
            .expect(2)
            .mount(server)
            .await;
        Mock::given(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn retries_429_503_and_empty_body() {
        let server = MockServer::start().await;
        flaky(&server, "429", ResponseTemplate::new(429)).await;
        flaky(&server, "503", ResponseTemplate::new(503)).await;
        flaky(
            &server,
            "empty",
            ResponseTemplate::new(200).set_body_string(" \n"),
        )
        .await;
        let rt = runtime(&getter("nil"));
        for name in ["429", "503", "empty"] {
            let (status, _, body) = get(&rt, format!("{}/{name}", server.uri())).await.unwrap();
            assert_eq!((status, body.as_str()), (200, "ok"), "{name}");
        }
    }

    #[tokio::test]
    async fn retry_empty_false_returns_empty_body() {
        let server = MockServer::start().await;
        Mock::given(path("/empty"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let rt = runtime(&getter("{ retry_empty = false }"));
        let (status, _, body) = get(&rt, format!("{}/empty", server.uri())).await.unwrap();
        assert_eq!((status, body.as_str()), (200, ""));
    }

    #[tokio::test]
    async fn honors_retry_after() {
        let server = MockServer::start().await;
        flaky(
            &server,
            "slow",
            ResponseTemplate::new(429).insert_header("Retry-After", "1"),
        )
        .await;
        let rt = runtime(&getter("nil"));
        let start = std::time::Instant::now();
        let (_, _, body) = get(&rt, format!("{}/slow", server.uri())).await.unwrap();
        assert_eq!(body, "ok");
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn persistent_throttle_is_rate_limit_error() {
        let server = MockServer::start().await;
        Mock::given(path("/busy"))
            .respond_with(ResponseTemplate::new(429))
            .expect(4)
            .mount(&server)
            .await;
        let rt = runtime(&getter("nil"));
        let err = get(&rt, format!("{}/busy", server.uri()))
            .await
            .unwrap_err();
        let err = err.to_string();
        assert!(
            err.starts_with("T: the site is rate limiting; try again in a few seconds"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn rate_limit_spaces_requests() {
        let server = MockServer::start().await;
        Mock::given(path("/r"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(5)
            .mount(&server)
            .await;
        let rt = runtime_with(
            10.0,
            BODY_CAP,
            r#"five = function(url) for _ = 1, 5 do http.get(url) end end,"#,
        );
        let start = std::time::Instant::now();
        rt.call::<_, ()>("five", format!("{}/r", server.uri()))
            .await
            .unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn body_over_cap_errors() {
        let server = MockServer::start().await;
        Mock::given(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 2048]))
            .mount(&server)
            .await;
        Mock::given(path("/small"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1024]))
            .mount(&server)
            .await;
        let rt = runtime_with(50.0, 1024, &getter("nil"));
        let err = get(&rt, format!("{}/big", server.uri())).await.unwrap_err();
        assert!(err.to_string().contains("larger than 1024 bytes"), "{err}");
        let (_, _, body) = get(&rt, format!("{}/small", server.uri())).await.unwrap();
        assert_eq!(body.len(), 1024);
    }

    #[tokio::test]
    async fn fetch_bytes_returns_body_and_rejects_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(path("/page.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
            .mount(&server)
            .await;
        Mock::given(path("/missing.png"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let http = Http::new(fast_retry(), BODY_CAP);
        let bytes = http
            .fetch_bytes(&format!("{}/page.png", server.uri()))
            .await
            .unwrap();
        assert_eq!(bytes, [1, 2, 3]);
        let err = http
            .fetch_bytes(&format!("{}/missing.png", server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn retry_after_reads_seconds_and_unix_time() {
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, v.parse().unwrap());
            }
            h
        };
        let now = 1_800_000_000;
        let wait = |pairs: &[(&'static str, &str)]| retry_after(&headers(pairs), now);
        assert_eq!(wait(&[]), None);
        assert_eq!(
            wait(&[("retry-after", "12")]),
            Some(Duration::from_secs(12))
        );
        assert_eq!(wait(&[("retry-after", "soon")]), None);
        assert_eq!(
            wait(&[("x-ratelimit-retry-after", "1800000009")]),
            Some(Duration::from_secs(9))
        );
    }
}
