//! Where a source repo lives (an HTTPS URL, a local folder, or a git remote) and fetching its
//! files. Nothing here trusts what it fetches: [`super::index::verify`] does the checks.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, anyhow, bail};
use reqwest::Url;

use crate::plugins::index::safe_rel_path;
use crate::plugins::runtime::sha256_hex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoLocation {
    Https { index_url: Url },
    Dir(PathBuf),
    Git { url: String, checkout: PathBuf },
}

/// A repo's `index.json` with its signature and key, as fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub index_bytes: Vec<u8>,
    /// Empty when the repo has no `index.json.minisig`.
    pub minisig: String,
    /// Empty when the repo has no `key.pub`.
    pub key_pub: String,
}

/// The most hon reads of one repo file.
#[derive(Clone, Copy)]
struct Cap {
    bytes: u64,
    label: &'static str,
}

const INDEX_CAP: Cap = Cap {
    bytes: 4 * 1024 * 1024,
    label: "4 MiB",
};
/// Each plugin or lib file.
const FILE_CAP: Cap = Cap {
    bytes: 4 * 1024 * 1024,
    label: "4 MiB",
};
/// `index.json.minisig` and `key.pub`.
const SMALL_CAP: Cap = Cap {
    bytes: 16 * 1024,
    label: "16 KiB",
};

const GIT_MISSING: &str =
    "`--git` needs git installed; install git or add the repo by HTTPS URL or folder";

/// Passed to every git run: no `ext::` remotes (they run commands), no hooks, and symlinks
/// checked out as plain files so a repo can't point hon at files outside the checkout.
const GIT_HARDENING: [&str; 6] = [
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.hooksPath=",
    "-c",
    "core.symlinks=false",
];

/// Variables that point git at some other repo or config: `git rev-parse --local-env-vars`,
/// plus `GIT_NAMESPACE`. Inherited, `GIT_DIR` and `GIT_WORK_TREE` beat `-C`, so `reset --hard`
/// would run on that repo.
const GIT_REPO_ENV: [&str; 16] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

const GIT_URL_FORMS: &str = "`--git` repo URLs must be https://, ssh://, file://, \
    user@host:path or a local path (plain http:// only for localhost)";

impl RepoLocation {
    /// Without `git`: an existing folder, or an `https://` URL of `index.json` or the folder
    /// holding it. With `git`: anything `git clone` accepts, checked out under `checkouts_dir`.
    pub fn parse(input: &str, git: bool, checkouts_dir: &Path) -> anyhow::Result<RepoLocation> {
        if git {
            check_git_url(input)?;
            let hash = sha256_hex(input.as_bytes());
            return Ok(RepoLocation::Git {
                url: input.to_string(),
                checkout: checkouts_dir.join(&hash[..16]),
            });
        }
        let path = Path::new(input);
        if path.is_dir() {
            let dir = std::path::absolute(path)
                .with_context(|| format!("could not resolve the folder `{input}`"))?;
            return Ok(RepoLocation::Dir(dir));
        }
        let mut url = Url::parse(input)
            .map_err(|_| anyhow!("`{input}` is not an existing folder or an https:// URL"))?;
        check_scheme(&url)?;
        if !url.username().is_empty() || url.password().is_some() {
            bail!("repo URLs can't hold a user name or password; hon sends no credentials");
        }
        if !url.path().ends_with(".json") {
            if !url.path().ends_with('/') {
                let dir = format!("{}/", url.path());
                url.set_path(&dir);
            }
            url = url.join("index.json")?;
        }
        Ok(RepoLocation::Https { index_url: url })
    }

    /// The index with its signature and key. A git repo is cloned or updated first. `http`
    /// is meant to be [`crate::sources::http_client`], which keeps no cookies.
    pub async fn fetch_index(&self, http: &reqwest::Client) -> anyhow::Result<Fetched> {
        if let RepoLocation::Git { url, checkout } = self {
            update_checkout(url, checkout, Stdio::inherit).await?;
        }
        let name = self.index_name();
        let index_bytes = self
            .read(http, &name, INDEX_CAP)
            .await?
            .with_context(|| format!("the repo has no `{name}`"))?;
        let sig_name = format!("{name}.minisig");
        let minisig = text(self.read(http, &sig_name, SMALL_CAP).await?, &sig_name)?;
        let key_pub = text(self.read(http, "key.pub", SMALL_CAP).await?, "key.pub")?;
        Ok(Fetched {
            index_bytes,
            minisig,
            key_pub,
        })
    }

    /// A plugin or lib file at `rel` (a path from the index) inside the repo. A git repo is
    /// read from its checkout as the last [`Self::fetch_index`] left it.
    pub async fn fetch_file(&self, http: &reqwest::Client, rel: &str) -> anyhow::Result<Vec<u8>> {
        if !safe_rel_path(rel) {
            bail!("{rel:?} is not a path inside the repo");
        }
        self.read(http, rel, FILE_CAP)
            .await?
            .with_context(|| format!("the repo has no `{rel}`"))
    }

    fn index_name(&self) -> String {
        match self {
            RepoLocation::Https { index_url } => index_url
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or("index.json")
                .to_string(),
            RepoLocation::Dir(_) | RepoLocation::Git { .. } => "index.json".to_string(),
        }
    }

    /// `None` when the repo has no such file.
    async fn read(
        &self,
        http: &reqwest::Client,
        rel: &str,
        cap: Cap,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            RepoLocation::Https { index_url } => get(http, index_url.join(rel)?, rel, cap).await,
            RepoLocation::Dir(base) | RepoLocation::Git { checkout: base, .. } => {
                let (path, name) = (base.join(rel), rel.to_string());
                tokio::task::spawn_blocking(move || read_capped(&path, &name, cap)).await?
            }
        }
    }
}

/// `https`, or plain `http` to this machine only.
fn check_scheme(url: &Url) -> anyhow::Result<()> {
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    match url.scheme() {
        "https" => Ok(()),
        "http" if local => Ok(()),
        _ => bail!("repo URLs must use https:// (plain http:// only for localhost), not `{url}`"),
    }
}

/// Allows only transports that are encrypted or stay on this machine. The first `repo add`
/// trusts the `key.pub` it fetches, so a plaintext `git://` would let anyone on the path swap
/// key and index. Mirrors git's own reading of the URL: no `:`, or a `/` before the first
/// `:`, is a local path; `host:path` with no `//` or `:` after the colon is scp-style ssh;
/// anything else is `<scheme>://` or a `<helper>::` form.
fn check_git_url(input: &str) -> anyhow::Result<()> {
    let refuse = || anyhow!("{GIT_URL_FORMS}, not `{input}`");
    match input.split_once(':') {
        None => return Ok(()),
        Some((before, _)) if before.contains('/') => return Ok(()),
        Some((before, after))
            if !before.is_empty() && !after.starts_with(':') && !after.starts_with("//") =>
        {
            return Ok(());
        }
        Some(_) => {}
    }
    let url = Url::parse(input).map_err(|_| refuse())?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    match url.scheme() {
        "https" | "ssh" | "file" => Ok(()),
        "http" if local => Ok(()),
        _ => Err(refuse()),
    }
}

/// Where a request ended up after redirects: still an allowed URL, and never plain http
/// when it started on https (not even to localhost).
fn check_redirect(asked: &Url, got: &Url) -> anyhow::Result<()> {
    check_scheme(got)?;
    if asked.scheme() == "https" && got.scheme() != "https" {
        bail!("{asked} redirected to {got}; repo URLs must stay on https://");
    }
    Ok(())
}

fn too_big(name: &str, cap: Cap) -> anyhow::Error {
    anyhow!("`{name}` in the repo is larger than {}", cap.label)
}

fn text(bytes: Option<Vec<u8>>, name: &str) -> anyhow::Result<String> {
    String::from_utf8(bytes.unwrap_or_default()).map_err(|_| anyhow!("`{name}` is not UTF-8 text"))
}

/// `None` on 404. Counts the body as it streams, so a missing or false Content-Length can't
/// get past `cap`.
async fn get(
    http: &reqwest::Client,
    url: Url,
    name: &str,
    cap: Cap,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut resp = http
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("could not fetch {url}"))?;
    check_redirect(&url, resp.url())?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("could not fetch {url}: HTTP {status}");
    }
    if resp.content_length().is_some_and(|n| n > cap.bytes) {
        return Err(too_big(name, cap));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("could not fetch {url}"))?
    {
        if (body.len() + chunk.len()) as u64 > cap.bytes {
            return Err(too_big(name, cap));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// `None` when `path` doesn't exist. Refuses anything but a regular file, so a FIFO or device
/// can't block or flood the read. The type is checked once before opening, because opening a
/// FIFO blocks, and the type and size again on the open handle, which is what gets read.
fn read_capped(path: &Path, name: &str, cap: Cap) -> anyhow::Result<Option<Vec<u8>>> {
    let not_a_file = || anyhow!("`{name}` in the repo is not a file");
    let read_err = || format!("could not read {}", path.display());
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(meta) if !meta.is_file() => return Err(not_a_file()),
        r => r.with_context(read_err)?,
    };
    let file = std::fs::File::open(path).with_context(read_err)?;
    let meta = file.metadata().with_context(read_err)?;
    if !meta.is_file() {
        return Err(not_a_file());
    }
    if meta.len() > cap.bytes {
        return Err(too_big(name, cap));
    }
    let mut bytes = Vec::new();
    file.take(cap.bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(read_err)?;
    if bytes.len() as u64 > cap.bytes {
        return Err(too_big(name, cap));
    }
    Ok(Some(bytes))
}

/// Clones on first use; after that fetches the remote's default branch and resets to it.
async fn update_checkout(url: &str, checkout: &Path, stderr: fn() -> Stdio) -> anyhow::Result<()> {
    if checkout.join(".git").is_dir() {
        let fetch = ["--quiet", "--depth", "1", "origin", "HEAD"];
        run_git(Some(checkout), "fetch", &fetch.map(OsStr::new), stderr).await?;
        let reset = ["--quiet", "--hard", "FETCH_HEAD"];
        return run_git(Some(checkout), "reset", &reset.map(OsStr::new), stderr).await;
    }
    if let Some(parent) = checkout.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let clone = ["--quiet", "--depth", "1", "--", url].map(OsStr::new);
    run_git(
        None,
        "clone",
        &[&clone[..], &[checkout.as_os_str()]].concat(),
        stderr,
    )
    .await
}

/// Runs `git [-C dir] <sub> <args>` with [`GIT_HARDENING`] and without [`GIT_REPO_ENV`].
/// stdin is inherited, and stderr is `stderr()` (inherited outside tests), so git can prompt
/// for credentials and show its own errors.
async fn run_git(
    dir: Option<&Path>,
    sub: &str,
    args: &[&OsStr],
    stderr: fn() -> Stdio,
) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("git");
    for var in GIT_REPO_ENV {
        cmd.env_remove(var);
    }
    cmd.args(GIT_HARDENING);
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    let status = cmd
        .arg(sub)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(stderr())
        .status()
        .await;
    let status = match status {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(GIT_MISSING),
        r => r.context("could not run git")?,
    };
    if !status.success() {
        bail!("`git {sub}` failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::process::Command;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::sources::http_client;

    fn index_url(input: &str) -> String {
        match RepoLocation::parse(input, false, Path::new("/unused")).unwrap() {
            RepoLocation::Https { index_url } => index_url.to_string(),
            other => panic!("{input}: {other:?}"),
        }
    }

    #[test]
    fn index_url_forms() {
        for input in ["https://h/r", "https://h/r/", "https://h/r/index.json"] {
            assert_eq!(index_url(input), "https://h/r/index.json", "{input}");
        }
        assert_eq!(index_url("https://h"), "https://h/index.json");
        assert_eq!(index_url("https://h/r/main.json"), "https://h/r/main.json");
        assert_eq!(
            index_url("http://127.0.0.1:9/r"),
            "http://127.0.0.1:9/r/index.json"
        );
        assert_eq!(
            index_url("http://localhost/r"),
            "http://localhost/r/index.json"
        );

        for bad in [
            "http://example.com/r",
            "ftp://h/r",
            "http://127.0.0.1.example.com/r",
        ] {
            let err = RepoLocation::parse(bad, false, Path::new("/unused"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("https"), "{bad}: {err}");
        }
        let err = RepoLocation::parse("https://u:p@h/r", false, Path::new("/unused"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("password"), "{err}");
        assert!(RepoLocation::parse("no/such/folder", false, Path::new("/unused")).is_err());
    }

    #[test]
    fn parse_dir_and_git() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().to_str().unwrap();
        assert_eq!(
            RepoLocation::parse(input, false, Path::new("/unused")).unwrap(),
            RepoLocation::Dir(std::path::absolute(dir.path()).unwrap())
        );

        let url = "git@github.com:owner/repo.git";
        let checkouts = Path::new("/data/checkouts");
        let RepoLocation::Git { url: got, checkout } =
            RepoLocation::parse(url, true, checkouts).unwrap()
        else {
            panic!("not git");
        };
        assert_eq!(got, url);
        let hash = crate::plugins::runtime::sha256_hex(url.as_bytes());
        assert_eq!(checkout, checkouts.join(&hash[..16]));
    }

    #[test]
    fn git_url_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let checkouts = tmp.path().join("checkouts");
        for good in [
            "git@github.com:o/r.git",
            "github.com:o/r",
            "ssh://git@host/r",
            "https://host/r.git",
            "file:///tmp/x",
            "http://127.0.0.1:9/r.git",
            "http://localhost/r.git",
            "/srv/repos/r.git",
            "../r.git",
        ] {
            assert!(
                RepoLocation::parse(good, true, &checkouts).is_ok(),
                "{good}"
            );
        }

        let marker = tmp.path().join("marker");
        let ext = format!("ext::sh -c touch% {}", marker.display());
        for bad in [
            "git://host/r.git",
            "ftp://host/r.git",
            "ftps://host/r.git",
            "http://example.com/r.git",
            "fd::7",
            "http::https://host/r.git",
            ext.as_str(),
        ] {
            let err = RepoLocation::parse(bad, true, &checkouts)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("https://") && err.contains("ssh://"),
                "{bad}: {err}"
            );
        }
        assert!(!marker.exists());
    }

    #[test]
    fn redirects_cannot_leave_https() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(check_redirect(&url("https://h/i.json"), &url("https://g/i.json")).is_ok());
        assert!(check_redirect(&url("http://localhost/i"), &url("http://127.0.0.1/i")).is_ok());
        for to in ["http://localhost/i", "http://127.0.0.1/i", "http://g/i"] {
            let err = check_redirect(&url("https://h/i.json"), &url(to)).unwrap_err();
            assert!(err.to_string().contains("https"), "{to}: {err}");
        }
        assert!(check_redirect(&url("http://localhost/i"), &url("http://g/i")).is_err());
    }

    fn write_repo(dir: &Path, version: &str) {
        std::fs::write(dir.join("index.json"), format!("{{\"v\":{version}}}")).unwrap();
        std::fs::write(dir.join("index.json.minisig"), format!("sig {version}")).unwrap();
        std::fs::write(dir.join("key.pub"), "key").unwrap();
        std::fs::create_dir_all(dir.join("sources")).unwrap();
        std::fs::write(dir.join("sources/a.lua"), format!("-- {version}")).unwrap();
    }

    #[tokio::test]
    async fn dir_repo_reads_files() {
        let dir = tempfile::tempdir().unwrap();
        write_repo(dir.path(), "1");
        let repo = RepoLocation::Dir(dir.path().to_path_buf());
        let http = http_client();
        let fetched = repo.fetch_index(&http).await.unwrap();
        assert_eq!(
            fetched,
            Fetched {
                index_bytes: b"{\"v\":1}".to_vec(),
                minisig: "sig 1".into(),
                key_pub: "key".into(),
            }
        );
        assert_eq!(
            repo.fetch_file(&http, "sources/a.lua").await.unwrap(),
            b"-- 1"
        );
        for bad in ["../x.lua", "/etc/passwd", "sources/missing.lua"] {
            assert!(repo.fetch_file(&http, bad).await.is_err(), "{bad}");
        }

        std::fs::remove_file(dir.path().join("index.json.minisig")).unwrap();
        std::fs::remove_file(dir.path().join("key.pub")).unwrap();
        let fetched = repo.fetch_index(&http).await.unwrap();
        assert_eq!(
            (fetched.minisig.as_str(), fetched.key_pub.as_str()),
            ("", "")
        );

        std::fs::remove_file(dir.path().join("index.json")).unwrap();
        let err = repo.fetch_index(&http).await.unwrap_err().to_string();
        assert!(err.contains("index.json"), "{err}");
    }

    #[tokio::test]
    async fn dir_repo_enforces_size_caps() {
        let dir = tempfile::tempdir().unwrap();
        write_repo(dir.path(), "1");
        let repo = RepoLocation::Dir(dir.path().to_path_buf());
        let http = http_client();

        std::fs::write(dir.path().join("key.pub"), vec![b'k'; 16 * 1024 + 1]).unwrap();
        let err = repo.fetch_index(&http).await.unwrap_err().to_string();
        assert!(err.contains("`key.pub`") && err.contains("16 KiB"), "{err}");

        std::fs::write(dir.path().join("key.pub"), vec![b'k'; 16 * 1024]).unwrap();
        std::fs::write(
            dir.path().join("index.json.minisig"),
            vec![b's'; 16 * 1024 + 1],
        )
        .unwrap();
        let err = repo.fetch_index(&http).await.unwrap_err().to_string();
        assert!(err.contains("`index.json.minisig`"), "{err}");

        std::fs::write(dir.path().join("index.json.minisig"), "sig").unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            vec![b' '; 4 * 1024 * 1024 + 1],
        )
        .unwrap();
        let err = repo.fetch_index(&http).await.unwrap_err().to_string();
        assert!(
            err.contains("`index.json`") && err.contains("4 MiB"),
            "{err}"
        );

        std::fs::write(
            dir.path().join("sources/a.lua"),
            vec![b'-'; 4 * 1024 * 1024 + 1],
        )
        .unwrap();
        let err = repo
            .fetch_file(&http, "sources/a.lua")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`sources/a.lua`"), "{err}");
    }

    #[tokio::test]
    async fn https_repo_fetches_three_files() {
        let server = MockServer::start().await;
        for (p, body) in [
            ("/r/index.json", "{\"v\":1}"),
            ("/r/index.json.minisig", "sig"),
            ("/r/key.pub", "key"),
            ("/r/sources/a.lua", "-- a"),
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        let repo =
            RepoLocation::parse(&format!("{}/r/", server.uri()), false, Path::new("/x")).unwrap();
        let http = http_client();
        let fetched = repo.fetch_index(&http).await.unwrap();
        assert_eq!(
            fetched,
            Fetched {
                index_bytes: b"{\"v\":1}".to_vec(),
                minisig: "sig".into(),
                key_pub: "key".into(),
            }
        );
        assert_eq!(
            repo.fetch_file(&http, "sources/a.lua").await.unwrap(),
            b"-- a"
        );
        assert!(repo.fetch_file(&http, "sources/none.lua").await.is_err());
        assert!(repo.fetch_file(&http, "../r/index.json").await.is_err());
    }

    #[tokio::test]
    async fn https_repo_missing_signature_and_key_are_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let repo = RepoLocation::parse(&server.uri(), false, Path::new("/x")).unwrap();
        let fetched = repo.fetch_index(&http_client()).await.unwrap();
        assert_eq!(
            (fetched.minisig.as_str(), fetched.key_pub.as_str()),
            ("", "")
        );
    }

    #[tokio::test]
    async fn https_repo_enforces_size_caps() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; 4 * 1024 * 1024 + 1]),
            )
            .mount(&server)
            .await;
        let repo = RepoLocation::parse(&server.uri(), false, Path::new("/x")).unwrap();
        let err = repo
            .fetch_index(&http_client())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`index.json`") && err.contains("4 MiB"),
            "{err}"
        );
    }

    /// Answers one request on `listener` with `head` and then `body_len` spaces.
    fn serve_once(listener: std::net::TcpListener, head: String, body_len: usize) {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let _ = conn.write_all(head.as_bytes());
            let chunk = vec![b' '; 64 * 1024];
            let mut left = body_len;
            while left > 0 {
                let n = left.min(chunk.len());
                if conn.write_all(&chunk[..n]).is_err() {
                    break;
                }
                left -= n;
            }
        });
    }

    /// A body with no Content-Length, so only the streamed count can catch it.
    #[tokio::test]
    async fn https_repo_caps_streamed_bodies() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let head = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_string();
        serve_once(listener, head, 5 * 1024 * 1024);
        let url = format!("http://127.0.0.1:{port}/");
        let repo = RepoLocation::parse(&url, false, Path::new("/x")).unwrap();
        let err = repo
            .fetch_index(&http_client())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`index.json`") && err.contains("4 MiB"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn https_repo_refuses_redirect_to_plain_http() {
        // Plain http on [::1] is not one of the allowed loopback names.
        let Ok(listener) = std::net::TcpListener::bind("[::1]:0") else {
            println!("skipping https_repo_refuses_redirect_to_plain_http: no IPv6 loopback");
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";
        serve_once(listener, head.to_string(), 2);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("http://[::1]:{port}/index.json")),
            )
            .mount(&server)
            .await;
        let repo = RepoLocation::parse(&server.uri(), false, Path::new("/x")).unwrap();
        let err = repo
            .fetch_index(&http_client())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("https"), "{err}");
    }

    fn git_on_path() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// Runs a test-side git command with an identity and no signing, whatever the user's config.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=hon",
                "-c",
                "user.email=hon@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn commit_and_push(work: &Path, version: &str) {
        write_repo(work, version);
        git(work, &["add", "-A"]);
        git(work, &["commit", "-q", "-m", version]);
        git(work, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    }

    #[tokio::test]
    async fn git_repo_clones_then_updates() {
        if !git_on_path() {
            println!("skipping git_repo_clones_then_updates: git is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let url = remote_with_v1(tmp.path());
        let repo = RepoLocation::parse(&url, true, &tmp.path().join("checkouts")).unwrap();
        let http = http_client();
        let fetched = repo.fetch_index(&http).await.unwrap();
        assert_eq!(fetched.index_bytes, b"{\"v\":1}");
        assert_eq!(fetched.minisig, "sig 1");

        commit_and_push(&tmp.path().join("work"), "2");
        let fetched = repo.fetch_index(&http).await.unwrap();
        assert_eq!(fetched.index_bytes, b"{\"v\":2}");
        assert_eq!(
            repo.fetch_file(&http, "sources/a.lua").await.unwrap(),
            b"-- 2"
        );
    }

    #[tokio::test]
    async fn git_failure_names_exit_status() {
        if !git_on_path() {
            println!("skipping git_failure_names_exit_status: git is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let missing = Url::from_directory_path(tmp.path().join("nothing.git")).unwrap();
        let checkout = tmp.path().join("checkouts/x");
        let err = update_checkout(missing.as_str(), &checkout, Stdio::null)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("git clone") && err.contains("exit status"),
            "{err}"
        );
    }

    /// Names the child test a [`run_child`] process is for.
    const CHILD: &str = "HON_TEST_CHILD";
    const CHILD_URL: &str = "HON_TEST_GIT_URL";
    const CHILD_CHECKOUTS: &str = "HON_TEST_CHECKOUTS";

    /// Runs the ignored test `name` in a fresh copy of this test binary with `envs` added, so
    /// no other test sees the changed environment.
    fn run_child(name: &str, envs: &[(&str, &OsStr)]) {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("plugins::repo::tests::{name}"),
                "--ignored",
            ])
            .env(CHILD, name)
            .envs(envs.iter().copied())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
    }

    /// Child tests do nothing unless [`run_child`] started them.
    fn is_child(name: &str) -> bool {
        std::env::var_os(CHILD).is_some_and(|v| v == name)
    }

    /// The git repo a child test was handed through [`CHILD_URL`] and [`CHILD_CHECKOUTS`].
    fn child_repo() -> RepoLocation {
        let url = std::env::var(CHILD_URL).unwrap();
        let checkouts = PathBuf::from(std::env::var_os(CHILD_CHECKOUTS).unwrap());
        RepoLocation::parse(&url, true, &checkouts).unwrap()
    }

    #[test]
    fn git_missing_gives_clear_error() {
        let empty = tempfile::tempdir().unwrap();
        run_child("git_missing_child", &[("PATH", empty.path().as_os_str())]);
    }

    #[tokio::test]
    #[ignore = "run by git_missing_gives_clear_error with an empty PATH"]
    async fn git_missing_child() {
        if !is_child("git_missing_child") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo =
            RepoLocation::parse("https://h/r.git", true, &tmp.path().join("checkouts")).unwrap();
        let err = repo.fetch_index(&http_client()).await.unwrap_err();
        assert_eq!(err.to_string(), GIT_MISSING);
    }

    /// A bare remote at `tmp/remote.git` with v1 pushed from `tmp/work`; returns its
    /// `file://` URL.
    fn remote_with_v1(tmp: &Path) -> String {
        let bare = tmp.join("remote.git");
        let work = tmp.join("work");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git(&bare, &["init", "-q", "--bare", "--initial-branch=main"]);
        git(&work, &["init", "-q", "--initial-branch=main"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        commit_and_push(&work, "1");
        Url::from_directory_path(&bare).unwrap().to_string()
    }

    /// With `GIT_DIR` and `GIT_WORK_TREE` pointing at another repo (as inside `vcsh enter`),
    /// the update must still act on hon's checkout, not reset that repo.
    #[tokio::test]
    async fn git_ignores_repo_env_vars() {
        if !git_on_path() {
            println!("skipping git_ignores_repo_env_vars: git is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let url = remote_with_v1(tmp.path());
        let checkouts = tmp.path().join("checkouts");
        RepoLocation::parse(&url, true, &checkouts)
            .unwrap()
            .fetch_index(&http_client())
            .await
            .unwrap();
        commit_and_push(&tmp.path().join("work"), "2");

        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "-q", "--initial-branch=main"]);
        git(&other, &["remote", "add", "origin", &url]);
        std::fs::write(other.join("notes.txt"), "committed").unwrap();
        git(&other, &["add", "-A"]);
        git(&other, &["commit", "-q", "-m", "notes"]);
        std::fs::write(other.join("notes.txt"), "uncommitted").unwrap();

        run_child(
            "git_env_child",
            &[
                ("GIT_DIR", other.join(".git").as_os_str()),
                ("GIT_WORK_TREE", other.as_os_str()),
                (CHILD_URL, OsStr::new(&url)),
                (CHILD_CHECKOUTS, checkouts.as_os_str()),
            ],
        );
        assert_eq!(
            std::fs::read_to_string(other.join("notes.txt")).unwrap(),
            "uncommitted"
        );
    }

    #[tokio::test]
    #[ignore = "run by git_ignores_repo_env_vars with GIT_DIR set"]
    async fn git_env_child() {
        if !is_child("git_env_child") {
            return;
        }
        let fetched = child_repo().fetch_index(&http_client()).await.unwrap();
        assert_eq!(fetched.index_bytes, b"{\"v\":2}");
    }

    /// A `post-checkout` hook in the user's global `core.hooksPath` must not run on clone.
    #[cfg(unix)]
    #[tokio::test]
    async fn git_skips_global_hooks() {
        use std::os::unix::fs::PermissionsExt;

        if !git_on_path() {
            println!("skipping git_skips_global_hooks: git is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let url = remote_with_v1(tmp.path());
        let hooks = tmp.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let marker = tmp.path().join("hook-ran");
        let hook = hooks.join("post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = tmp.path().join("gitconfig");
        std::fs::write(
            &config,
            format!("[core]\n\thooksPath = {}\n", hooks.display()),
        )
        .unwrap();

        run_child(
            "git_hooks_child",
            &[
                ("GIT_CONFIG_GLOBAL", config.as_os_str()),
                (CHILD_URL, OsStr::new(&url)),
                (CHILD_CHECKOUTS, tmp.path().join("checkouts").as_os_str()),
            ],
        );
        assert!(!marker.exists(), "the global post-checkout hook ran");
    }

    #[tokio::test]
    #[ignore = "run by git_skips_global_hooks with a global hooks config"]
    async fn git_hooks_child() {
        if !is_child("git_hooks_child") {
            return;
        }
        let fetched = child_repo().fetch_index(&http_client()).await.unwrap();
        assert_eq!(fetched.index_bytes, b"{\"v\":1}");
    }

    /// A symlink committed to the repo arrives as a plain file holding the link text, so it
    /// can't point hon at a file outside the checkout.
    #[cfg(unix)]
    #[tokio::test]
    async fn git_checks_out_symlinks_as_files() {
        if !git_on_path() {
            println!("skipping git_checks_out_symlinks_as_files: git is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let url = remote_with_v1(tmp.path());
        let work = tmp.path().join("work");
        std::os::unix::fs::symlink("/etc/hosts", work.join("sources/link.lua")).unwrap();
        commit_and_push(&work, "2");

        let repo = RepoLocation::parse(&url, true, &tmp.path().join("checkouts")).unwrap();
        let http = http_client();
        repo.fetch_index(&http).await.unwrap();
        let RepoLocation::Git { checkout, .. } = &repo else {
            unreachable!()
        };
        let meta = std::fs::symlink_metadata(checkout.join("sources/link.lua")).unwrap();
        assert!(meta.is_file(), "{meta:?}");
        assert_eq!(
            repo.fetch_file(&http, "sources/link.lua").await.unwrap(),
            b"/etc/hosts"
        );
    }
}
