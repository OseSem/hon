use std::io::Cursor;
use std::path::Path;

use minisign::KeyPair;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::plugins::runtime::sha256_hex;
use crate::sources::http_client;

/// A local-folder repo, signed with its own key on every `publish`.
struct Repo {
    dir: TempDir,
    kp: KeyPair,
    name: String,
    sources: Vec<Value>,
    libs: Vec<Value>,
}

impl Repo {
    fn new(name: &str) -> Repo {
        Repo {
            dir: tempfile::tempdir().unwrap(),
            kp: KeyPair::generate_unencrypted_keypair().unwrap(),
            name: name.to_string(),
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
    fn source(&mut self, id: &str, version: u32, code: &str, extra: Value) -> &mut Repo {
        let file = format!("sources/{id}.lua");
        self.write(&file, code.as_bytes());
        let mut entry = json!({
            "id": id, "name": id.to_uppercase(), "version": version, "lang": "en",
            "nsfw": false, "api": 1, "file": file, "sha256": sha256_hex(code.as_bytes()),
            "libs": [],
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

    /// Unlists lib `name` and deletes its file.
    fn drop_lib(&mut self, name: &str) -> &mut Repo {
        let file = format!("lib/{name}.lua");
        self.libs.retain(|l| l["file"] != file);
        for s in &mut self.sources {
            s["libs"] = json!([]);
        }
        std::fs::remove_file(self.dir.path().join(file)).unwrap();
        self
    }

    /// Writes `index.json` signed with the repo's key, and `key.pub`.
    fn publish(&self) {
        let index = json!({
            "name": self.name, "api": 1, "sources": self.sources, "libs": self.libs,
        });
        let bytes = serde_json::to_vec_pretty(&index).unwrap();
        let sig = minisign::sign(
            Some(&self.kp.pk),
            &self.kp.sk,
            Cursor::new(&bytes),
            None,
            None,
        )
        .unwrap()
        .to_string();
        self.write("index.json", &bytes);
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

/// Plugin `id` whose search returns one result titled by lib `util`'s `tag()`, with the
/// site's `/search` body as its id.
fn plugin(id: &str, base: &str) -> String {
    format!(
        r#"local util = require("util")
        return {{
            id = "{id}", name = "{upper}", lang = "en", api = 1, requests_per_second = 50,
            search = function(query)
                local body = http.get("{base}/search").body
                return {{ {{ id = body, title = util.tag(), url = "{base}/m" }} }}
            end,
            chapters = function(manga) return {{}} end,
            pages = function(chapter) return {{}} end,
        }}"#,
        upper = id.to_uppercase(),
    )
}

fn util(tag: &str) -> String {
    format!(r#"return {{ tag = function() return "{tag}" end }}"#)
}

async fn site() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string("m1"))
        .mount(&server)
        .await;
    server
}

/// A repo offering plugin `t` (which needs lib `util`, tag "v1"), published.
fn repo_with_t(base: &str) -> Repo {
    let mut repo = Repo::new("Test repo");
    repo.lib("util", &util("v1")).source(
        "t",
        1,
        &plugin("t", base),
        json!({ "libs": ["lib/util.lua"] }),
    );
    repo.publish();
    repo
}

struct Env {
    db: Db,
    plugins: TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            db: Db::open_in_memory().unwrap(),
            plugins: tempfile::tempdir().unwrap(),
        }
    }

    fn store(&self) -> Store<'_> {
        Store::new(&self.db, self.plugins.path(), http_client())
    }

    fn installed_file(&self, id: &str) -> PathBuf {
        let row = self.row(id);
        self.plugins
            .path()
            .join(row.repo_id.to_string())
            .join("sources")
            .join(format!("{id}.lua"))
    }

    fn row(&self, id: &str) -> crate::db::PluginRow {
        self.db
            .plugin_sources()
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap()
    }
}

/// `update(id)`'s changed ids, after checking no repo failed.
async fn updated(store: &Store<'_>, id: Option<&str>) -> Vec<String> {
    let report = store.update(id).await.unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    report.changed
}

async fn first_title(sources: &[Arc<dyn Source>]) -> anyhow::Result<String> {
    let found = sources[0].search("x").await?;
    Ok(found[0].title.clone())
}

fn status_of(store: &Store, id: &str) -> String {
    let rows = store.list_sources(true).unwrap();
    let row = rows.iter().find(|r| r.id == id).unwrap();
    row.status.to_string()
}

#[tokio::test]
async fn add_repo_pins_key_and_lists() {
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t("http://unused");
    let info = store.add_repo(repo.path(), false, None).await.unwrap();
    let location = std::path::absolute(repo.dir.path())
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let want = RepoInfo {
        name: "Test repo".into(),
        location,
        pubkey: repo.pubkey(),
    };
    assert_eq!(info, want);
    assert_eq!(store.list_repos().unwrap(), std::slice::from_ref(&want));

    let err = store.add_repo(repo.path(), false, None).await.unwrap_err();
    assert!(err.to_string().contains("already added"), "{err}");

    let rows = store.list_sources(false).unwrap();
    assert_eq!(
        rows,
        [SourceRow {
            id: "t".into(),
            name: "T".into(),
            lang: "en".into(),
            repo: "Test repo".into(),
            installed: None,
            available: Some(1),
            status: Status::Available,
        }]
    );

    store.remove_repo("Test repo").unwrap();
    assert!(store.list_repos().unwrap().is_empty());
    assert!(store.list_sources(true).unwrap().is_empty());
}

#[tokio::test]
async fn add_repo_with_wrong_key_flag_refused() {
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t("http://unused");
    let other = KeyPair::generate_unencrypted_keypair().unwrap();
    let err = store
        .add_repo(repo.path(), false, Some(&other.pk.to_base64()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("--key"), "{err}");
    assert!(store.list_repos().unwrap().is_empty());

    // The matching key is accepted.
    let info = store
        .add_repo(repo.path(), false, Some(&repo.pubkey()))
        .await
        .unwrap();
    assert_eq!(info.pubkey, repo.pubkey());
}

#[tokio::test]
async fn add_repo_without_key_pub_needs_key_flag() {
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t("http://unused");
    std::fs::remove_file(repo.dir.path().join("key.pub")).unwrap();
    let err = store.add_repo(repo.path(), false, None).await.unwrap_err();
    let location = std::path::absolute(repo.dir.path()).unwrap();
    assert_eq!(
        err.to_string(),
        format!(
            "{} has no key.pub; add it with --key <base64>",
            location.display()
        )
    );
    let info = store
        .add_repo(repo.path(), false, Some(&repo.pubkey()))
        .await
        .unwrap();
    assert_eq!(info.pubkey, repo.pubkey());
    store.install("t").await.unwrap();
}

#[tokio::test]
async fn install_loads_as_source() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t(&server.uri());
    store.add_repo(repo.path(), false, None).await.unwrap();
    assert!(store.load_installed().unwrap().is_empty());

    store.install("t").await.unwrap();
    assert_eq!(status_of(&store, "t"), "installed");
    let sources = store.load_installed().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id().slug(), "t");
    let found = sources[0].search("x").await.unwrap();
    assert_eq!(found[0].id, "m1");
    assert_eq!(found[0].title, "v1");

    store.remove("t").unwrap();
    assert!(store.load_installed().unwrap().is_empty());
    assert!(!env.plugins.path().join("1/sources/t.lua").exists());
    assert_eq!(status_of(&store, "t"), "available");
    assert!(store.remove("t").is_err());
}

#[tokio::test]
async fn install_refuses_non_en_and_newer_api() {
    let env = Env::new();
    let store = env.store();
    let mut repo = Repo::new("R");
    repo.source(
        "fr",
        1,
        &plugin("fr", "http://unused"),
        json!({ "lang": "fr" }),
    )
    .source(
        "new",
        1,
        &plugin("new", "http://unused"),
        json!({ "api": 2 }),
    );
    repo.publish();
    store.add_repo(repo.path(), false, None).await.unwrap();

    assert_eq!(status_of(&store, "fr"), "language not supported yet");
    assert_eq!(status_of(&store, "new"), "needs a newer hon");
    let err = store.install("fr").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "can't install fr: language not supported yet"
    );
    let err = store.install("new").await.unwrap_err();
    assert_eq!(err.to_string(), "can't install new: needs a newer hon");
    assert!(env.db.plugin_sources().unwrap().is_empty());
}

#[test]
fn status_strings() {
    let all = [
        (Status::Installed, "installed"),
        (Status::UpdateAvailable, "update available"),
        (Status::Available, "available"),
        (Status::NeedsNewerHon, "needs a newer hon"),
        (Status::LanguageNotSupported, "language not supported yet"),
        (Status::RemovedFromRepo, "removed from repo"),
    ];
    for (status, text) in all {
        assert_eq!(status.to_string(), text);
    }
}

#[tokio::test]
async fn nsfw_hidden_unless_flag() {
    let env = Env::new();
    let store = env.store();
    let mut repo = Repo::new("R");
    repo.source("safe", 1, &plugin("safe", "http://unused"), json!({}))
        .source(
            "lewd",
            1,
            &plugin("lewd", "http://unused"),
            json!({ "nsfw": true }),
        );
    repo.publish();
    store.add_repo(repo.path(), false, None).await.unwrap();

    let ids = |nsfw| -> Vec<String> {
        store
            .list_sources(nsfw)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    };
    assert_eq!(ids(false), ["safe"]);
    assert_eq!(ids(true), ["lewd", "safe"]);
}

#[tokio::test]
async fn update_moves_sources_and_libs_together() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let mut repo = repo_with_t(&server.uri());
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    assert!(updated(&store, None).await.is_empty());

    let bumped = format!("-- v2\n{}", plugin("t", &server.uri()));
    repo.lib("util", &util("v2"))
        .source("t", 2, &bumped, json!({ "libs": ["lib/util.lua"] }));
    repo.publish();
    // The stored index is refreshed only by a sync.
    assert_eq!(status_of(&store, "t"), "installed");

    assert_eq!(updated(&store, None).await, ["t"]);
    let row = env.row("t");
    assert_eq!(row.version, 2);
    assert_eq!(row.sha256, sha256_hex(bumped.as_bytes()));
    assert_eq!(status_of(&store, "t"), "installed");
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v2"
    );
    assert_eq!(updated(&store, Some("t")).await, Vec::<String>::new());
}

#[tokio::test]
async fn update_available_shows_after_refresh() {
    let env = Env::new();
    let store = env.store();
    let mut repo = Repo::new("R");
    let code = plugin("t", "http://unused");
    repo.lib("util", &util("v1"))
        .source("t", 1, &code, json!({}));
    repo.publish();
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();

    // An update that needs a newer hon keeps the installed version.
    repo.source("t", 2, &format!("--\n{code}"), json!({ "api": 2 }));
    repo.publish();
    assert!(updated(&store, None).await.is_empty());
    let rows = store.list_sources(false).unwrap();
    assert_eq!((rows[0].installed, rows[0].available), (Some(1), Some(2)));
    assert_eq!(rows[0].status, Status::NeedsNewerHon);
    assert_eq!(store.load_installed().unwrap().len(), 1);
    assert_eq!(env.row("t").version, 1);
}

#[tokio::test]
async fn removed_from_repo_keeps_working() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let mut repo = repo_with_t(&server.uri());
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();

    repo.drop_source("t").publish();
    assert!(updated(&store, None).await.is_empty());
    assert_eq!(status_of(&store, "t"), "removed from repo");
    assert!(env.row("t").removed_from_repo);
    let rows = store.list_sources(false).unwrap();
    assert_eq!((rows[0].installed, rows[0].available), (Some(1), None));
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v1"
    );

    // The repo drops the lib too: the kept source keeps its copy.
    repo.drop_lib("util").publish();
    assert!(updated(&store, None).await.is_empty());
    let repo_id = env.row("t").repo_id;
    assert!(env.db.plugin_libs(repo_id).unwrap().contains_key("util"));
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v1"
    );

    // Once nothing is kept, the dropped lib goes.
    store.remove("t").unwrap();
    assert!(updated(&store, None).await.is_empty());
    assert!(env.db.plugin_libs(repo_id).unwrap().is_empty());
    assert!(
        !env.plugins
            .path()
            .join(format!("{repo_id}/lib/util.lua"))
            .exists()
    );
}

#[tokio::test]
async fn update_reports_each_repo() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let mut a = repo_with_t(&server.uri());
    let mut b = Repo::new("B");
    b.lib("util", &util("b1"))
        .source("u", 1, &plugin("u", &server.uri()), json!({}));
    b.publish();
    let a_info = store.add_repo(a.path(), false, None).await.unwrap();
    store.add_repo(b.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    store.install("u").await.unwrap();

    a.kp = KeyPair::generate_unencrypted_keypair().unwrap();
    a.source(
        "t",
        2,
        &format!("--\n{}", plugin("t", &server.uri())),
        json!({}),
    );
    a.publish();
    b.lib("util", &util("b2")).source(
        "u",
        2,
        &format!("--\n{}", plugin("u", &server.uri())),
        json!({}),
    );
    b.publish();

    let report = store.update(None).await.unwrap();
    let at = &a_info.location;
    assert_eq!(
        report,
        UpdateReport {
            changed: vec!["u".into()],
            failed: vec![(
                at.clone(),
                format!(
                    "{at}'s signing key changed; if you trust the new key, run \
                     `hon repo remove {at}` and add it again"
                )
            )],
        }
    );
    assert_eq!(env.row("t").version, 1);
    assert_eq!(env.row("u").version, 2);

    // One id: only its repo syncs, and its failure is reported the same way.
    let report = store.update(Some("t")).await.unwrap();
    assert_eq!(report.changed, Vec::<String>::new());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, *at);
    let err = store.update(Some("nope")).await.unwrap_err();
    assert_eq!(err.to_string(), "nope is not installed");
}

#[tokio::test]
async fn load_installed_errs_when_tables_unreadable() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("hon.db");
    let db = Db::open(&db_path).unwrap();
    let store = Store::new(&db, tmp.path().join("plugins"), http_client());
    let repo = repo_with_t("http://unused");
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    assert_eq!(store.load_installed().unwrap().len(), 1);
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE plugin_source;")
        .unwrap();
    let err = store.load_installed().err().unwrap();
    assert!(
        format!("{err:#}").contains("reading the installed plugins"),
        "{err:#}"
    );
}

/// The database refusing a sync after the new folder is in place puts the old one back.
#[tokio::test]
async fn failed_commit_after_swap_restores_folder() {
    let env = Env::new();
    let store = env.store();
    let a = repo_with_t("http://unused");
    let mut b = Repo::new("B");
    b.lib("util", &util("b"));
    b.publish();
    store.add_repo(a.path(), false, None).await.unwrap();
    store.add_repo(b.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    assert!(updated(&store, None).await.is_empty());
    let a_row = env.row("t");
    let a_file = env.installed_file("t");
    let a_bytes = std::fs::read(&a_file).unwrap();

    let b_row = env.db.plugin_repos().unwrap()[1].clone();
    let b_dir = env.plugins.path().join(b_row.id.to_string());
    let b_before = folder_files(&b_dir);
    assert_eq!(b_before, ["lib/util.lua"]);

    b.source("t", 1, &plugin("t", "http://unused"), json!({}));
    b.publish();
    let err = store.sync_repo(&b_row, Some("t")).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("t is installed from another repo"),
        "{err:#}"
    );
    assert_eq!(folder_files(&b_dir), b_before);
    for leftover in [".old", ".tmp"] {
        let path = env.plugins.path().join(format!("{}{leftover}", b_row.id));
        assert!(!path.exists(), "{}", path.display());
    }
    assert_eq!(env.row("t"), a_row);
    assert_eq!(std::fs::read(&a_file).unwrap(), a_bytes);
    assert_eq!(
        env.db.plugin_repos().unwrap()[1].index_json,
        b_row.index_json
    );
}

/// A sync that died with the live folder moved aside is picked up by the next one.
#[tokio::test]
async fn sync_recovers_folder_left_aside() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t(&server.uri());
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    let repo_id = env.row("t").repo_id;
    let live = env.plugins.path().join(repo_id.to_string());
    let old = env.plugins.path().join(format!("{repo_id}.old"));
    std::fs::rename(&live, &old).unwrap();
    // Simulate a repo that is now unreachable: the folder still comes back, from `.old`.
    std::fs::rename(repo.dir.path().join("lib"), repo.dir.path().join("lib.x")).unwrap();
    let report = store.update(None).await.unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(live.is_dir() && !old.exists());
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v1"
    );

    std::fs::rename(repo.dir.path().join("lib.x"), repo.dir.path().join("lib")).unwrap();
    std::fs::rename(&live, &old).unwrap();
    assert!(updated(&store, None).await.is_empty());
    assert!(live.is_dir() && !old.exists());
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v1"
    );
}

/// Every file under `dir`, relative and sorted.
fn folder_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for sub in std::fs::read_dir(dir).unwrap() {
        let sub = sub.unwrap().path();
        for f in std::fs::read_dir(&sub).unwrap() {
            let f = f.unwrap().path();
            out.push(f.strip_prefix(dir).unwrap().to_str().unwrap().to_string());
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn changed_key_refused() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let mut repo = repo_with_t(&server.uri());
    let info = store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    let file = env.installed_file("t");
    let before = std::fs::read(&file).unwrap();

    repo.kp = KeyPair::generate_unencrypted_keypair().unwrap();
    repo.lib("util", &util("v2")).source(
        "t",
        2,
        &format!("--\n{}", plugin("t", &server.uri())),
        json!({}),
    );
    repo.publish();
    let report = store.update(None).await.unwrap();
    let repo_name = &info.location;
    assert_eq!(report.changed, Vec::<String>::new());
    assert_eq!(
        report.failed,
        [(
            repo_name.clone(),
            format!(
                "{repo_name}'s signing key changed; if you trust the new key, run \
                 `hon repo remove {repo_name}` and add it again"
            )
        )]
    );
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(env.row("t").version, 1);
    assert_eq!(store.list_repos().unwrap()[0].pubkey, info.pubkey);
}

#[tokio::test]
async fn failed_sync_changes_nothing() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let mut repo = repo_with_t(&server.uri());
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    let file = env.installed_file("t");
    let before = std::fs::read(&file).unwrap();
    let row = env.row("t");
    let index_before = env.db.plugin_repos().unwrap()[0].index_json.clone();

    repo.source(
        "t",
        2,
        &format!("--\n{}", plugin("t", &server.uri())),
        json!({}),
    );
    repo.lib("gone", "return {}");
    std::fs::remove_file(repo.dir.path().join("lib/gone.lua")).unwrap();
    repo.publish();
    let report = store.update(Some("t")).await.unwrap();
    assert!(report.changed.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].1.contains("lib/gone.lua"), "{report:?}");

    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(env.row("t"), row);
    assert_eq!(env.db.plugin_repos().unwrap()[0].index_json, index_before);
    assert_eq!(
        env.db
            .plugin_libs(row.repo_id)
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["util"]
    );
    assert!(
        !env.plugins
            .path()
            .join(format!("{}.tmp", row.repo_id))
            .exists()
    );
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v1"
    );
}

#[tokio::test]
async fn bad_hash_in_repo_refused() {
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t("http://unused");
    store.add_repo(repo.path(), false, None).await.unwrap();
    repo.write("sources/t.lua", b"return {}");
    let err = store.install("t").await.unwrap_err();
    assert!(format!("{err:#}").contains("hash"), "{err:#}");
    assert!(env.db.plugin_sources().unwrap().is_empty());
}

#[tokio::test]
async fn tampered_file_on_disk_becomes_broken() {
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t(&server.uri());
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();

    let file = env.installed_file("t");
    let mut code = std::fs::read_to_string(&file).unwrap();
    code.push_str("\n-- edited");
    std::fs::write(&file, code).unwrap();
    let sources = store.load_installed().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id().slug(), "t");
    let err = sources[0].search("x").await.unwrap_err();
    assert!(err.to_string().contains("hash"), "{err}");

    std::fs::remove_file(&file).unwrap();
    let sources = store.load_installed().unwrap();
    assert!(sources[0].search("x").await.is_err());

    // A tampered lib fails too.
    store.install("t").await.unwrap();
    let lib = file
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("lib/util.lua");
    std::fs::write(&lib, util("evil")).unwrap();
    let err = first_title(&store.load_installed().unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("hash"), "{err}");
}

#[tokio::test]
async fn duplicate_id_across_repos_refused() {
    let env = Env::new();
    let store = env.store();
    let a = repo_with_t("http://unused");
    let mut b = Repo::new("Other repo");
    b.lib("util", &util("b"))
        .source("t", 1, &plugin("t", "http://unused"), json!({}));
    b.publish();
    let a_info = store.add_repo(a.path(), false, None).await.unwrap();
    let b_info = store.add_repo(b.path(), false, None).await.unwrap();

    let err = store.install("t").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "t is in more than one repo ({}, {}); remove one",
            a_info.location, b_info.location
        )
    );

    store.remove_repo(&b_info.location).unwrap();
    store.install("t").await.unwrap();
    store.add_repo(b.path(), false, None).await.unwrap();
    let err = store.install("t").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "t is in more than one repo ({}, {}); remove one",
            a_info.location, b_info.location
        )
    );

    // Installed from a, and only b lists it now.
    let mut a = a;
    a.drop_source("t").publish();
    updated(&store, None).await;
    let err = store.install("t").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("t is already installed from {}", a_info.location)
    );
}

#[tokio::test]
async fn remove_repo_deletes_its_files() {
    let env = Env::new();
    let store = env.store();
    let repo = repo_with_t("http://unused");
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    let folder = env.installed_file("t");
    let folder = folder.parent().unwrap().parent().unwrap().to_path_buf();
    assert!(folder.is_dir());
    assert!(store.remove_repo("no such repo").is_err());
    store.remove_repo("Test repo").unwrap();
    assert!(!folder.exists());
    assert!(env.db.plugin_sources().unwrap().is_empty());
    assert!(store.load_installed().unwrap().is_empty());
}

#[tokio::test]
async fn wrong_manifest_id_becomes_broken() {
    let env = Env::new();
    let store = env.store();
    let mut repo = Repo::new("R");
    repo.lib("util", &util("v1"))
        .source("t", 1, &plugin("mangadex", "http://unused"), json!({}));
    repo.publish();
    store.add_repo(repo.path(), false, None).await.unwrap();
    store.install("t").await.unwrap();
    let sources = store.load_installed().unwrap();
    assert_eq!(sources[0].id().slug(), "t");
    let err = sources[0].search("x").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "T: the plugin calls itself `mangadex`, but its repo lists it as `t`"
    );
}

/// Runs a test-side git command with an identity and no signing, whatever the user's
/// config.
fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=hon", "-c", "user.email=hon@localhost"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn commit_all(dir: &Path) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "publish"]);
}

#[tokio::test]
async fn git_repo_is_stored_by_url_and_cloned_fresh() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipping git_repo_is_stored_by_url_and_cloned_fresh: git is not on PATH");
        return;
    }
    let server = site().await;
    let env = Env::new();
    let store = env.store();
    let mut repo = repo_with_t(&server.uri());
    git(repo.dir.path(), &["init", "-q", "--initial-branch=main"]);
    commit_all(repo.dir.path());

    let url = format!("file://{}", repo.path());
    let info = store.add_repo(&url, true, None).await.unwrap();
    assert_eq!(info.location, format!("git:{url}"));
    assert_eq!(env.db.plugin_repos().unwrap()[0].kind, "git");
    store.install("t").await.unwrap();
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v1"
    );

    // An update reads the new commit, not a stale checkout.
    repo.lib("util", &util("v2")).source(
        "t",
        2,
        &format!("--\n{}", plugin("t", &server.uri())),
        json!({}),
    );
    repo.publish();
    commit_all(repo.dir.path());
    assert_eq!(updated(&store, Some("t")).await, ["t"]);
    assert_eq!(
        first_title(&store.load_installed().unwrap()).await.unwrap(),
        "v2"
    );

    let checkouts = env.plugins.path().join("checkouts");
    assert_eq!(std::fs::read_dir(&checkouts).unwrap().count(), 1);
    store.remove_repo(&info.location).unwrap();
    assert_eq!(std::fs::read_dir(&checkouts).unwrap().count(), 0);
}

#[test]
fn installed_paths_come_from_ids_not_index_files() {
    let folder = Path::new("/p/3");
    assert_eq!(
        source_file(folder, "t").unwrap(),
        Path::new("/p/3/sources/t.lua")
    );
    assert!(source_file(folder, "../t").is_err());
    assert_eq!(lib_name("lib/util.lua"), Some("util"));
    assert!(lib_file(folder, "../x").is_err());
    assert_eq!(
        lib_file(folder, "util").unwrap(),
        Path::new("/p/3/lib/util.lua")
    );
    for bad in [
        "lib/../x.lua",
        "lib/a/b.lua",
        "lib/.lua",
        "x/util.lua",
        "lib/U.lua",
    ] {
        assert_eq!(lib_name(bad), None, "{bad}");
    }
}
