use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Output};

use hon_core::plugins::runtime::sha256_hex;
use minisign::KeyPair;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NO_SOURCES: &str =
    "error: No sources installed. Add a source repo with `hon repo add <url>`.\n";

fn hon(args: &[&str]) -> (tempfile::TempDir, Output) {
    let home = tempfile::tempdir().unwrap();
    let output = hon_in(home.path(), args);
    (home, output)
}

/// Runs hon with `HON_HOME=home`.
fn hon_in(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hon"))
        .args(args)
        .env("HON_HOME", home)
        .output()
        .unwrap()
}

/// hon in `home`, which must succeed; its stdout.
fn ok(home: &Path, args: &[&str]) -> String {
    let out = hon_in(home, args);
    assert!(
        out.status.success(),
        "hon {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// A local-folder repo, signed with its own key on every `publish`.
struct Repo {
    dir: TempDir,
    kp: KeyPair,
    sources: Vec<Value>,
    libs: Vec<Value>,
}

impl Repo {
    fn new() -> Repo {
        Repo {
            dir: tempfile::tempdir().unwrap(),
            kp: KeyPair::generate_unencrypted_keypair().unwrap(),
            sources: Vec::new(),
            libs: Vec::new(),
        }
    }

    fn path(&self) -> &str {
        self.dir.path().to_str().unwrap()
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Writes `sources/<id>.lua` and lists it (replacing an earlier entry for `id`).
    fn source(&mut self, id: &str, code: &str, extra: Value) -> &mut Repo {
        let file = format!("sources/{id}.lua");
        self.write(&file, code.as_bytes());
        let mut entry = json!({
            "id": id, "name": id.to_uppercase(), "version": 1, "lang": "en",
            "nsfw": false, "api": 1, "file": file, "sha256": sha256_hex(code.as_bytes()),
            "libs": ["lib/util.lua"],
        });
        for (k, v) in extra.as_object().unwrap() {
            entry[k] = v.clone();
        }
        self.sources.retain(|s| s["id"] != id);
        self.sources.push(entry);
        self
    }

    fn lib(&mut self, name: &str, code: &str) -> &mut Repo {
        let file = format!("lib/{name}.lua");
        self.write(&file, code.as_bytes());
        self.libs.retain(|l| l["file"] != file);
        self.libs
            .push(json!({ "file": file, "sha256": sha256_hex(code.as_bytes()) }));
        self
    }

    fn drop_source(&mut self, id: &str) -> &mut Repo {
        self.sources.retain(|s| s["id"] != id);
        self
    }

    /// Writes `index.json` signed with the repo's key, and `key.pub`.
    fn publish(&self) {
        let index = json!({
            "name": "Test sources", "api": 1, "sources": self.sources, "libs": self.libs,
        });
        self.write("index.json", &serde_json::to_vec_pretty(&index).unwrap());
        self.sign();
    }

    /// Signs the `index.json` that's there and writes `key.pub`.
    fn sign(&self) {
        let bytes = std::fs::read(self.dir.path().join("index.json")).unwrap();
        let sig = minisign::sign(
            Some(&self.kp.pk),
            &self.kp.sk,
            Cursor::new(&bytes),
            None,
            None,
        )
        .unwrap()
        .to_string();
        self.write("index.json.minisig", sig.as_bytes());
        self.write(
            "key.pub",
            self.kp.pk.to_box().unwrap().to_string().as_bytes(),
        );
    }

    fn pubkey(&self) -> String {
        self.kp.pk.to_base64()
    }
}

/// Plugin `id`: its search returns one result whose id is the site's `/search` body and whose
/// title comes from lib `util`; one chapter; one page.
fn plugin(id: &str, base: &str) -> String {
    format!(
        r#"local util = require("util")
        return {{
            id = "{id}", name = "{upper}", lang = "en", api = 1, requests_per_second = 50,
            search = function(query)
                log.debug("searching " .. query)
                local body = http.get("{base}/search").body
                return {{ {{ id = body, title = util.title(), url = "{base}/m" }} }}
            end,
            chapters = function(manga)
                return {{ {{ id = manga.id .. "-c1", number = 1, title = "One" }} }}
            end,
            pages = function(chapter) return {{ "{base}/" .. chapter.id .. ".jpg" }} end,
        }}"#,
        upper = id.to_uppercase(),
    )
}

const UTIL: &str = r#"return { title = function() return "Solo Leveling" end }"#;

async fn site() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string("m1"))
        .mount(&server)
        .await;
    server
}

#[test]
fn help_lists_commands() {
    let (_home, out) = hon(&["--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let commands: Vec<&str> = stdout
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    for cmd in ["search", "download", "read", "repo", "source"] {
        assert!(commands.contains(&cmd), "{cmd} missing from:\n{stdout}");
    }
    assert!(!commands.contains(&"man"), "{stdout}");
}

#[test]
fn no_sources_says_how_to_add_one() {
    for args in [
        &["search", "x"][..],
        &["download", "x", "1"],
        &["download", "x", "1", "--source", "foo"],
        &["read", "x"],
        &["read", "x", "3", "--source", "foo"],
    ] {
        let (_home, out) = hon(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(stderr(&out), NO_SOURCES, "{args:?}");
    }
}

#[test]
fn interactive_mode_needs_a_terminal() {
    let (_home, out) = hon(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "error: hon needs an interactive terminal (try `hon search`)\n"
    );
}

#[test]
fn bad_config_reports_cause() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "colour = \"red\"\n").unwrap();
    let out = hon_in(home.path(), &["search", "x"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = stderr(&out);
    assert!(stderr.starts_with("error: "), "{stderr}");
    assert!(stderr.contains("colour"), "{stderr}");
}

#[test]
fn man_page_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../man/hon.1");
    let committed = std::fs::read(path).unwrap_or_default();
    assert!(
        committed == hon::cli::man_page(),
        "man/hon.1 is out of date; run `cargo run -- man > man/hon.1`"
    );
}

#[test]
fn repo_add_list_remove() {
    let home = tempfile::tempdir().unwrap();
    let mut repo = Repo::new();
    repo.lib("util", UTIL)
        .source("alpha", &plugin("alpha", "http://127.0.0.1:9"), json!({}));
    repo.publish();

    let added = ok(home.path(), &["repo", "add", repo.path()]);
    assert_eq!(
        added,
        format!(
            "Added Test sources ({})\nSigning key: {}\n",
            repo.path(),
            repo.pubkey()
        )
    );
    let again = hon_in(home.path(), &["repo", "add", repo.path()]);
    assert!(!again.status.success());
    assert!(
        stderr(&again).contains("already added"),
        "{}",
        stderr(&again)
    );

    let listed = ok(home.path(), &["repo", "list"]);
    assert_eq!(listed, format!("Test sources  {}\n", repo.path()));

    ok(home.path(), &["repo", "remove", "Test sources"]);
    assert_eq!(ok(home.path(), &["repo", "list"]), "");
    let gone = hon_in(home.path(), &["repo", "remove", "Test sources"]);
    assert!(!gone.status.success());
    assert!(
        stderr(&gone).contains("no repo is named"),
        "{}",
        stderr(&gone)
    );
}

#[test]
fn repo_add_with_wrong_key_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let mut repo = Repo::new();
    repo.lib("util", UTIL)
        .source("alpha", &plugin("alpha", "http://127.0.0.1:9"), json!({}));
    repo.publish();
    let other = KeyPair::generate_unencrypted_keypair().unwrap();
    let out = hon_in(
        home.path(),
        &["repo", "add", repo.path(), "--key", &other.pk.to_base64()],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("doesn't match"), "{}", stderr(&out));
}

#[tokio::test(flavor = "multi_thread")]
async fn source_install_then_search_uses_it() {
    let server = site().await;
    let home = tempfile::tempdir().unwrap();
    let mut repo = Repo::new();
    repo.lib("util", UTIL)
        .source("alpha", &plugin("alpha", &server.uri()), json!({}));
    repo.publish();
    ok(home.path(), &["repo", "add", repo.path()]);

    let missing = hon_in(home.path(), &["source", "install", "zeta"]);
    assert!(!missing.status.success());
    assert_eq!(
        stderr(&missing),
        "error: no added repo offers `zeta`; run `hon source update` to refresh the repo lists\n"
    );

    assert_eq!(
        ok(home.path(), &["source", "install", "alpha"]),
        "Installed alpha\n"
    );
    let found = ok(home.path(), &["search", "solo"]);
    assert!(found.starts_with("Solo Leveling\n  ALPHA "), "{found}");
    assert!(found.contains("1 ch   latest 1"), "{found}");

    let unknown = hon_in(home.path(), &["download", "x", "1", "--source", "foo"]);
    assert!(!unknown.status.success());
    assert!(
        stderr(&unknown).contains("unknown source \"foo\"; installed: alpha"),
        "{}",
        stderr(&unknown)
    );

    assert_eq!(
        ok(home.path(), &["source", "remove", "alpha"]),
        "Removed alpha\n"
    );
    let out = hon_in(home.path(), &["search", "solo"]);
    assert_eq!(stderr(&out), NO_SOURCES);
}

#[test]
fn source_list_statuses() {
    let home = tempfile::tempdir().unwrap();
    let base = "http://127.0.0.1:9";
    let mut repo = Repo::new();
    repo.lib("util", UTIL)
        .source("alpha", &plugin("alpha", base), json!({}))
        .source("beta", &plugin("beta", base), json!({ "version": 3 }))
        .source("gamma", &plugin("gamma", base), json!({ "api": 2 }))
        .source("delta", &plugin("delta", base), json!({ "lang": "ja" }))
        .source("epsilon", &plugin("epsilon", base), json!({ "nsfw": true }));
    repo.publish();
    ok(home.path(), &["repo", "add", repo.path()]);
    ok(home.path(), &["source", "install", "alpha"]);
    let refused = hon_in(home.path(), &["source", "install", "gamma"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("can't install gamma: needs a newer hon"),
        "{}",
        stderr(&refused)
    );

    assert_eq!(
        ok(home.path(), &["source", "list"]),
        "alpha  ALPHA  en  v1  Test sources  installed\n\
         beta   BETA   en  v3  Test sources  available\n\
         delta  DELTA  ja  v1  Test sources  language not supported yet\n\
         gamma  GAMMA  en  v1  Test sources  needs a newer hon\n"
    );
    let with_nsfw = ok(home.path(), &["source", "list", "--nsfw"]);
    assert!(
        with_nsfw.contains("epsilon  EPSILON  en  v1  Test sources  available\n"),
        "{with_nsfw}"
    );

    assert_eq!(
        ok(home.path(), &["source", "update"]),
        "everything is up to date\n"
    );
    repo.source("alpha", &plugin("alpha", base), json!({ "version": 2 }));
    repo.publish();
    assert_eq!(ok(home.path(), &["source", "update"]), "updated alpha\n");

    repo.drop_source("alpha");
    repo.publish();
    ok(home.path(), &["source", "update", "alpha"]);
    let listed = ok(home.path(), &["source", "list"]);
    assert!(
        listed.starts_with("alpha  ALPHA  en  v2  Test sources  removed from repo\n"),
        "{listed}"
    );

    std::fs::remove_file(repo.dir.path().join("index.json")).unwrap();
    let failed = hon_in(home.path(), &["source", "update"]);
    assert!(!failed.status.success());
    let err = stderr(&failed);
    assert!(
        err.starts_with(&format!("warning: {}: ", repo.path())),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn source_test_runs_file_without_installing() {
    let server = site().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sources")).unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::write(dir.path().join("lib/util.lua"), UTIL).unwrap();
    let file = dir.path().join("sources/alpha.lua");
    std::fs::write(&file, plugin("alpha", &server.uri())).unwrap();
    let file = file.to_str().unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = hon_in(home.path(), &["source", "test", file, "--search", "solo"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8(out.stdout.clone()).unwrap(),
        format!("m1  Solo Leveling  {}/m\n", server.uri())
    );
    assert_eq!(stderr(&out), "alpha: searching solo\n");

    assert_eq!(
        ok(home.path(), &["source", "test", file, "--chapters", "m1"]),
        "1  m1-c1  One\n"
    );
    assert_eq!(
        ok(home.path(), &["source", "test", file, "--pages", "m1-c1"]),
        format!("{}/m1-c1.jpg\n", server.uri())
    );
    assert_eq!(
        ok(home.path(), &["source", "list"]),
        "",
        "nothing installed"
    );

    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("util.lua"), UTIL).unwrap();
    let lone = elsewhere.path().join("alpha.lua");
    std::fs::write(&lone, plugin("alpha", &server.uri())).unwrap();
    let lone = lone.to_str().unwrap();
    let lib_dir = elsewhere.path().to_str().unwrap();
    let args = [
        "source",
        "test",
        lone,
        "--lib-dir",
        lib_dir,
        "--search",
        "x",
    ];
    assert!(ok(home.path(), &args).starts_with("m1  "));

    let broken = dir.path().join("sources/broken.lua");
    std::fs::write(&broken, "return {").unwrap();
    let out = hon_in(
        home.path(),
        &["source", "test", broken.to_str().unwrap(), "--search", "x"],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.starts_with("error: broken.lua: "), "{err}");
    assert!(err.contains("Expected"), "{err}");

    let no_mode = hon_in(home.path(), &["source", "test", file]);
    assert!(!no_mode.status.success());
}

#[test]
fn source_index_writes_hashes() {
    let home = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let alpha = plugin("alpha", "http://127.0.0.1:9");
    let beta = r#"return {
        id = "beta", name = "Beta", lang = "en", api = 1, version = 4, nsfw = true,
        search = function() return {} end,
        chapters = function() return {} end,
        pages = function() return {} end,
    }"#;
    repo.write("sources/alpha.lua", alpha.as_bytes());
    repo.write("sources/beta.lua", beta.as_bytes());
    repo.write("sources/notes.txt", b"not a plugin");
    repo.write("lib/util.lua", UTIL.as_bytes());
    repo.write("lib/unused.lua", b"return {}");
    repo.write("lib/Not-A-Module.lua", b"return {}");

    let out = ok(home.path(), &["source", "index", repo.path()]);
    let index_path = repo.dir.path().join("index.json");
    let shown = index_path.display().to_string();
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some(shown.as_str()));
    assert_eq!(
        lines.next(),
        Some(format!("sign it: minisign -Sm {shown}").as_str())
    );

    let index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    let name = repo.dir.path().file_name().unwrap().to_str().unwrap();
    assert_eq!(
        index,
        json!({
            "name": name,
            "api": 1,
            "sources": [
                {
                    "id": "alpha", "name": "ALPHA", "version": 1, "lang": "en", "nsfw": false,
                    "api": 1, "file": "sources/alpha.lua",
                    "sha256": sha256_hex(alpha.as_bytes()), "libs": ["lib/util.lua"],
                },
                {
                    "id": "beta", "name": "Beta", "version": 4, "lang": "en", "nsfw": true,
                    "api": 1, "file": "sources/beta.lua",
                    "sha256": sha256_hex(beta.as_bytes()), "libs": [],
                },
            ],
            "libs": [
                { "file": "lib/unused.lua", "sha256": sha256_hex(b"return {}") },
                { "file": "lib/util.lua", "sha256": sha256_hex(UTIL.as_bytes()) },
            ],
        })
    );

    // A second run keeps the name the index has.
    let mut renamed = index.clone();
    renamed["name"] = json!("My sources");
    std::fs::write(&index_path, serde_json::to_vec(&renamed).unwrap()).unwrap();
    ok(home.path(), &["source", "index", repo.path()]);
    let again: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(again["name"], "My sources");

    // Signed, it's a repo hon installs from.
    repo.sign();
    ok(home.path(), &["repo", "add", repo.path()]);
    ok(home.path(), &["source", "install", "alpha"]);
}
