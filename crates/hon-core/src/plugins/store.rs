//! Installed source plugins: the repos the user added (rows in the database) and the plugin
//! and lib files fetched from them, under the plugins folder:
//! `<dir>/<repo_id>/sources/<id>.lua` and `<dir>/<repo_id>/lib/<name>.lua`. Paths are built
//! only from the repo's row id and validated ids and lib names, never from the index's `file`
//! strings, which are only used to fetch.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};

use crate::db::{Db, PluginRepo, PluginRow};
use crate::model::SourceId;
use crate::plugins::index::{self, Index, IndexSource};
use crate::plugins::repo::RepoLocation;
use crate::plugins::runtime::{API_VERSION, LibSource, display_text, sha256_hex};
use crate::plugins::source::{BrokenSource, LoadOpts, PluginSource, valid_id};
use crate::sources::Source;

/// The one language v1 installs.
const SUPPORTED_LANG: &str = "en";

pub struct Store<'a> {
    db: &'a Db,
    dir: PathBuf,
    http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub name: String,
    /// The index URL, the absolute folder path, or `git:<url>`.
    pub location: String,
    /// The pinned minisign key, base64.
    pub pubkey: String,
}

/// A source a repo offers, or one installed from a repo that no longer lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub id: String,
    pub name: String,
    pub lang: String,
    /// The repo's name.
    pub repo: String,
    pub installed: Option<u32>,
    pub available: Option<u32>,
    pub status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Installed,
    UpdateAvailable,
    Available,
    NeedsNewerHon,
    LanguageNotSupported,
    RemovedFromRepo,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Installed => "installed",
            Status::UpdateAvailable => "update available",
            Status::Available => "available",
            Status::NeedsNewerHon => "needs a newer hon",
            Status::LanguageNotSupported => "language not supported yet",
            Status::RemovedFromRepo => "removed from repo",
        })
    }
}

/// What the files staged for a sync add up to in the database.
struct Staged {
    rows: Vec<PluginRow>,
    libs: Vec<(String, String)>,
    changed: Vec<String>,
}

impl<'a> Store<'a> {
    /// `http` is meant to be [`crate::sources::http_client`], which keeps no cookies.
    pub fn new(db: &'a Db, plugins_dir: impl Into<PathBuf>, http: reqwest::Client) -> Store<'a> {
        Store {
            db,
            dir: plugins_dir.into(),
            http,
        }
    }

    /// Adds a repo and pins its key: the repo's `key.pub`, or `key` when the repo has none.
    /// When both are there they must match.
    pub async fn add_repo(
        &self,
        input: &str,
        git: bool,
        key: Option<&str>,
    ) -> anyhow::Result<RepoInfo> {
        let loc = RepoLocation::parse(input, git, &self.checkouts())?;
        let (kind, location) = canonical(&loc)?;
        if self
            .db
            .plugin_repos()?
            .iter()
            .any(|r| r.location == location)
        {
            bail!("{location} is already added");
        }
        let fetched = loc.fetch_index(&self.http).await?;
        let key = key.map(key_flag).transpose()?;
        let pubkey = if fetched.key_pub.trim().is_empty() {
            key.with_context(|| format!("{location} has no key.pub; add it with --key <base64>"))?
        } else {
            let theirs = index::pubkey_from_file(&fetched.key_pub)?;
            if key.is_some_and(|k| k != theirs) {
                bail!("the key given with --key doesn't match {location}'s key.pub");
            }
            theirs
        };
        let index = index::verify(&fetched.index_bytes, &fetched.minisig, &pubkey)?;
        let index_json = serde_json::to_string(&index)?;
        self.db
            .add_plugin_repo(&location, kind, &pubkey, &index.name, &index_json)?;
        Ok(RepoInfo {
            name: index.name,
            location,
            pubkey,
        })
    }

    pub fn list_repos(&self) -> anyhow::Result<Vec<RepoInfo>> {
        Ok(self
            .db
            .plugin_repos()?
            .into_iter()
            .map(|r| RepoInfo {
                name: r.name,
                location: r.location,
                pubkey: r.pubkey,
            })
            .collect())
    }

    /// Removes a repo, named by its location or its name, with everything installed from it.
    pub fn remove_repo(&self, location_or_name: &str) -> anyhow::Result<()> {
        let repos = self.db.plugin_repos()?;
        let repo = match repos.iter().find(|r| r.location == location_or_name) {
            Some(repo) => repo,
            None => match repos
                .iter()
                .filter(|r| r.name == location_or_name)
                .collect::<Vec<_>>()[..]
            {
                [repo] => repo,
                [] => bail!("no repo is named `{location_or_name}`"),
                _ => {
                    bail!("more than one repo is named `{location_or_name}`; remove it by location")
                }
            },
        };
        self.db.remove_plugin_repo(repo.id)?;
        remove_dir_if_exists(&self.repo_dir(repo.id))?;
        if let Ok(RepoLocation::Git { checkout, .. }) = self.location(repo) {
            remove_dir_if_exists(&checkout)?;
        }
        Ok(())
    }

    /// Every source the repos offer and every installed one, as of the last sync, sorted
    /// by id. NSFW sources only with `show_nsfw`.
    pub fn list_sources(&self, show_nsfw: bool) -> anyhow::Result<Vec<SourceRow>> {
        let repos = self.db.plugin_repos()?;
        let installed = self.db.plugin_sources()?;
        let mut out = Vec::new();
        for repo in &repos {
            let index = stored_index(repo)?;
            let in_repo = |id: &str| {
                installed
                    .iter()
                    .find(|r| r.repo_id == repo.id && r.id == id)
            };
            for e in &index.sources {
                if e.nsfw && !show_nsfw {
                    continue;
                }
                let row = in_repo(&e.id);
                let status = match row {
                    Some(r) if r.version == e.version && r.sha256 == e.sha256 => Status::Installed,
                    Some(_) => blocked(e).unwrap_or(Status::UpdateAvailable),
                    None => blocked(e).unwrap_or(Status::Available),
                };
                out.push(SourceRow {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    lang: e.lang.clone(),
                    repo: repo.name.clone(),
                    installed: row.map(|r| r.version),
                    available: Some(e.version),
                    status,
                });
            }
            for r in installed.iter().filter(|r| r.repo_id == repo.id) {
                if index.sources.iter().any(|e| e.id == r.id) || (r.nsfw && !show_nsfw) {
                    continue;
                }
                out.push(SourceRow {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    lang: r.lang.clone(),
                    repo: repo.name.clone(),
                    installed: Some(r.version),
                    available: None,
                    status: Status::RemovedFromRepo,
                });
            }
        }
        out.sort_by(|a, b| (&a.id, &a.repo).cmp(&(&b.id, &b.repo)));
        Ok(out)
    }

    /// Installs source `id` from the one repo that offers it, refreshing that repo first.
    /// Installing an installed source syncs its repo again.
    pub async fn install(&self, id: &str) -> anyhow::Result<()> {
        let repos = self.db.plugin_repos()?;
        let mut offering = Vec::new();
        for repo in &repos {
            if stored_index(repo)?.sources.iter().any(|s| s.id == id) {
                offering.push(repo);
            }
        }
        let repo = match offering[..] {
            [repo] => repo,
            [] => bail!("no added repo offers `{id}`"),
            _ => {
                let locations: Vec<_> = offering.iter().map(|r| r.location.as_str()).collect();
                bail!(
                    "{id} is in more than one repo ({}); remove one",
                    locations.join(", ")
                );
            }
        };
        if let Some(row) = self.installed(id)?
            && row.repo_id != repo.id
        {
            let from = repos
                .iter()
                .find(|r| r.id == row.repo_id)
                .map_or("another repo", |r| r.location.as_str());
            bail!("{id} is already installed from {from}");
        }
        self.sync_repo(repo, Some(id)).await?;
        Ok(())
    }

    /// Syncs the repo of installed source `id`, or every repo. Returns the ids whose
    /// installed version or file changed.
    pub async fn update(&self, id: Option<&str>) -> anyhow::Result<Vec<String>> {
        let repos = self.db.plugin_repos()?;
        let mut changed = Vec::new();
        match id {
            Some(id) => {
                let row = self
                    .installed(id)?
                    .with_context(|| format!("{id} is not installed"))?;
                let repo = repos
                    .iter()
                    .find(|r| r.id == row.repo_id)
                    .with_context(|| format!("{id}'s repo is gone"))?;
                changed = self.sync_repo(repo, None).await?;
            }
            None => {
                for repo in &repos {
                    changed.extend(self.sync_repo(repo, None).await?);
                }
            }
        }
        changed.sort();
        Ok(changed)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let row = self
            .installed(id)?
            .with_context(|| format!("{id} is not installed"))?;
        self.db.remove_plugin_source(id)?;
        let path = source_file(&self.repo_dir(row.repo_id), &row.id)?;
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// One source per installed plugin, sorted by id. A plugin that can't be loaded (a
    /// missing or changed file, a load error) comes back as a [`BrokenSource`] saying why.
    /// Must run inside a tokio runtime. Empty when the database can't be read.
    pub fn load_installed(&self) -> Vec<Arc<dyn Source>> {
        let (Ok(rows), Ok(repos)) = (self.db.plugin_sources(), self.db.plugin_repos()) else {
            return Vec::new();
        };
        rows.iter()
            .map(|row| match self.load_one(row, &repos) {
                Ok(source) => Arc::new(source) as Arc<dyn Source>,
                Err(e) => Arc::new(BrokenSource::new(
                    SourceId::new(&row.id, &row.name),
                    display_text(&format!("{e:#}")),
                )),
            })
            .collect()
    }

    fn load_one(&self, row: &PluginRow, repos: &[PluginRepo]) -> anyhow::Result<PluginSource> {
        let name = &row.name;
        if !repos.iter().any(|r| r.id == row.repo_id) {
            bail!("{name}: the repo it was installed from is gone; install it again");
        }
        let folder = self.repo_dir(row.repo_id);
        let path = source_file(&folder, &row.id)?;
        let code = std::fs::read(&path).map_err(|e| {
            anyhow!("{name}: can't read the installed plugin file ({e}); reinstall it")
        })?;
        if sha256_hex(&code) != row.sha256 {
            bail!("{name}: the installed plugin file doesn't match its hash; reinstall it");
        }
        let code = String::from_utf8(code)
            .map_err(|_| anyhow!("{name}: the plugin file is not UTF-8 text"))?;
        let libs = LibSource::Verified {
            dir: folder.join("lib"),
            hashes: self.db.plugin_libs(row.repo_id)?,
        };
        let source = PluginSource::load(&code, &row.id, libs, LoadOpts::default())?;
        let declared = &source.manifest().id;
        if *declared != row.id {
            bail!(
                "{name}: the plugin calls itself `{declared}`, but its repo lists it as `{}`",
                row.id
            );
        }
        Ok(source)
    }

    /// Refreshes `repo` all-or-nothing: fetches and verifies the index against the pinned
    /// key, stages every installed source still listed (and `add`) and every lib in
    /// `<dir>/<repo_id>.tmp`, swaps it in, then records it in one transaction. Returns the
    /// ids whose version or file changed. Refusing `add` still syncs the rest.
    async fn sync_repo(&self, repo: &PluginRepo, add: Option<&str>) -> anyhow::Result<Vec<String>> {
        let loc = self.location(repo)?;
        let at = &repo.location;
        let fetched = loc
            .fetch_index(&self.http)
            .await
            .with_context(|| format!("fetching the index of {at}"))?;
        if !fetched.key_pub.trim().is_empty() {
            let theirs = index::pubkey_from_file(&fetched.key_pub)
                .with_context(|| format!("reading {at}'s key.pub"))?;
            if theirs != repo.pubkey {
                bail!(
                    "{at}'s signing key changed; if you trust the new key, run \
                     `hon repo remove {at}` and add it again"
                );
            }
        }
        let index = index::verify(&fetched.index_bytes, &fetched.minisig, &repo.pubkey)
            .map_err(|e| anyhow!("{at}: {e:#}"))?;

        let mut refusal = None;
        let mut added = None;
        if let Some(id) = add {
            match index.sources.iter().find(|s| s.id == id) {
                None => refusal = Some(anyhow!("{id} is no longer in {at}")),
                Some(e) => match blocked(e) {
                    Some(status) => refusal = Some(anyhow!("can't install {id}: {status}")),
                    None => added = Some(e),
                },
            }
        }

        let live = self.repo_dir(repo.id);
        let tmp = self.dir.join(format!("{}.tmp", repo.id));
        let old = self.dir.join(format!("{}.old", repo.id));
        remove_dir_if_exists(&tmp)?;
        let staged = match self.stage(repo, &loc, &index, added, &live, &tmp).await {
            Ok(staged) => staged,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        };

        remove_dir_if_exists(&old)?;
        let had_live = live.exists();
        if had_live {
            std::fs::rename(&live, &old)
                .with_context(|| format!("moving {} aside", live.display()))?;
        }
        if let Err(e) = std::fs::rename(&tmp, &live) {
            if had_live {
                let _ = std::fs::rename(&old, &live);
            }
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e).with_context(|| format!("moving {} into place", tmp.display()));
        }
        let index_json = serde_json::to_string(&index)?;
        if let Err(e) = self.db.sync_plugin_repo(
            repo.id,
            &index.name,
            &index_json,
            &staged.rows,
            &staged.libs,
        ) {
            let _ = std::fs::remove_dir_all(&live);
            if had_live {
                let _ = std::fs::rename(&old, &live);
            }
            return Err(e);
        }
        let _ = std::fs::remove_dir_all(&old);
        if let Some(e) = refusal {
            return Err(e);
        }
        Ok(staged.changed)
    }

    /// Fills `tmp` with the files a sync of `repo` to `index` leaves installed.
    async fn stage(
        &self,
        repo: &PluginRepo,
        loc: &RepoLocation,
        index: &Index,
        add: Option<&IndexSource>,
        live: &Path,
        tmp: &Path,
    ) -> anyhow::Result<Staged> {
        std::fs::create_dir_all(tmp.join("sources"))
            .and_then(|()| std::fs::create_dir_all(tmp.join("lib")))
            .with_context(|| format!("creating {}", tmp.display()))?;
        let installed: Vec<PluginRow> = self
            .db
            .plugin_sources()?
            .into_iter()
            .filter(|r| r.repo_id == repo.id)
            .collect();
        let mut staged = Staged {
            rows: Vec::new(),
            libs: Vec::new(),
            changed: Vec::new(),
        };
        for row in &installed {
            match index.sources.iter().find(|s| s.id == row.id) {
                Some(e) if blocked(e).is_none() => {
                    self.download(repo, loc, &e.file, &e.sha256, &source_file(tmp, &e.id)?)
                        .await?;
                    if (e.version, &e.sha256) != (row.version, &row.sha256) {
                        staged.changed.push(e.id.clone());
                    }
                    staged.rows.push(installed_row(repo, e));
                }
                // Gone from the index, or its new version can't run here: keep what's there.
                listed => {
                    let (from, to) = (source_file(live, &row.id)?, source_file(tmp, &row.id)?);
                    match std::fs::copy(&from, &to) {
                        Err(e) if e.kind() != ErrorKind::NotFound => {
                            return Err(e).with_context(|| format!("copying {}", from.display()));
                        }
                        _ => {}
                    }
                    staged.rows.push(PluginRow {
                        removed_from_repo: listed.is_none(),
                        ..row.clone()
                    });
                }
            }
        }
        if let Some(e) = add.filter(|e| !installed.iter().any(|r| r.id == e.id)) {
            self.download(repo, loc, &e.file, &e.sha256, &source_file(tmp, &e.id)?)
                .await?;
            staged.changed.push(e.id.clone());
            staged.rows.push(installed_row(repo, e));
        }
        for lib in &index.libs {
            let name = lib_name(&lib.file)
                .with_context(|| format!("{:?} is not a lib file name", lib.file))?;
            self.download(repo, loc, &lib.file, &lib.sha256, &lib_file(tmp, name))
                .await?;
            staged.libs.push((name.to_string(), lib.sha256.clone()));
        }
        Ok(staged)
    }

    /// Fetches `rel` from the repo, checks it against `sha256` and writes it to `to`.
    async fn download(
        &self,
        repo: &PluginRepo,
        loc: &RepoLocation,
        rel: &str,
        sha256: &str,
        to: &Path,
    ) -> anyhow::Result<()> {
        let at = &repo.location;
        let bytes = loc
            .fetch_file(&self.http, rel)
            .await
            .with_context(|| format!("fetching `{rel}` from {at}"))?;
        if sha256_hex(&bytes) != sha256 {
            bail!("`{rel}` in {at} doesn't match its hash in the signed index");
        }
        std::fs::write(to, bytes).with_context(|| format!("writing {}", to.display()))
    }

    fn installed(&self, id: &str) -> anyhow::Result<Option<PluginRow>> {
        Ok(self.db.plugin_sources()?.into_iter().find(|r| r.id == id))
    }

    /// The repo's location rebuilt from its stored form (never a stored checkout path).
    fn location(&self, repo: &PluginRepo) -> anyhow::Result<RepoLocation> {
        let checkouts = self.checkouts();
        let loc = match repo.kind.as_str() {
            "git" => {
                let url = repo
                    .location
                    .strip_prefix("git:")
                    .with_context(|| format!("{} is not a git location", repo.location))?;
                RepoLocation::parse(url, true, &checkouts)?
            }
            _ => RepoLocation::parse(&repo.location, false, &checkouts)?,
        };
        let (kind, location) = canonical(&loc)?;
        if kind != repo.kind || location != repo.location {
            bail!(
                "{} no longer reads as the repo that was added; remove it and add it again",
                repo.location
            );
        }
        Ok(loc)
    }

    fn checkouts(&self) -> PathBuf {
        self.dir.join("checkouts")
    }

    fn repo_dir(&self, repo_id: i64) -> PathBuf {
        self.dir.join(repo_id.to_string())
    }
}

/// The `kind` and `location` a repo is stored under.
fn canonical(loc: &RepoLocation) -> anyhow::Result<(&'static str, String)> {
    Ok(match loc {
        RepoLocation::Https { index_url } => ("https", index_url.to_string()),
        RepoLocation::Dir(dir) => (
            "dir",
            dir.to_str()
                .with_context(|| format!("the folder {} is not a UTF-8 path", dir.display()))?
                .to_string(),
        ),
        RepoLocation::Git { url, .. } => ("git", format!("git:{url}")),
    })
}

/// The base64 key a `--key` value names.
fn key_flag(key: &str) -> anyhow::Result<String> {
    let key = key.trim();
    minisign_verify::PublicKey::from_base64(key)
        .map_err(|e| anyhow!("--key is not a minisign public key: {e}"))?;
    Ok(key.to_string())
}

/// The last verified index of `repo`, as stored.
fn stored_index(repo: &PluginRepo) -> anyhow::Result<Index> {
    serde_json::from_str(&repo.index_json)
        .with_context(|| format!("the stored index of {} is damaged", repo.location))
}

/// Why `e` can't be installed, if it can't.
fn blocked(e: &IndexSource) -> Option<Status> {
    if e.api > API_VERSION {
        Some(Status::NeedsNewerHon)
    } else if e.lang != SUPPORTED_LANG {
        Some(Status::LanguageNotSupported)
    } else {
        None
    }
}

fn installed_row(repo: &PluginRepo, e: &IndexSource) -> PluginRow {
    PluginRow {
        id: e.id.clone(),
        repo_id: repo.id,
        name: e.name.clone(),
        version: e.version,
        lang: e.lang.clone(),
        nsfw: e.nsfw,
        file: e.file.clone(),
        sha256: e.sha256.clone(),
        removed_from_repo: false,
    }
}

/// `<folder>/sources/<id>.lua`, for a valid id only.
fn source_file(folder: &Path, id: &str) -> anyhow::Result<PathBuf> {
    if !valid_id(id) {
        bail!("{id:?} is not a valid source id");
    }
    Ok(folder.join("sources").join(format!("{id}.lua")))
}

/// `<folder>/lib/<name>.lua`; `name` comes from [`lib_name`].
fn lib_file(folder: &Path, name: &str) -> PathBuf {
    folder.join("lib").join(format!("{name}.lua"))
}

/// `<name>` of `lib/<name>.lua`, when it is made of lowercase letters, digits and `_`.
fn lib_name(file: &str) -> Option<&str> {
    file.strip_prefix("lib/")
        .and_then(|f| f.strip_suffix(".lua"))
        .filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn remove_dir_if_exists(dir: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
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
        assert!(store.load_installed().is_empty());

        store.install("t").await.unwrap();
        assert_eq!(status_of(&store, "t"), "installed");
        let sources = store.load_installed();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id().slug(), "t");
        let found = sources[0].search("x").await.unwrap();
        assert_eq!(found[0].id, "m1");
        assert_eq!(found[0].title, "v1");

        store.remove("t").unwrap();
        assert!(store.load_installed().is_empty());
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

    #[tokio::test]
    async fn status_strings() {
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
        assert!(store.update(None).await.unwrap().is_empty());

        let bumped = format!("-- v2\n{}", plugin("t", &server.uri()));
        repo.lib("util", &util("v2"))
            .source("t", 2, &bumped, json!({ "libs": ["lib/util.lua"] }));
        repo.publish();
        // The stored index is refreshed only by a sync.
        assert_eq!(status_of(&store, "t"), "installed");

        assert_eq!(store.update(None).await.unwrap(), ["t"]);
        let row = env.row("t");
        assert_eq!(row.version, 2);
        assert_eq!(row.sha256, sha256_hex(bumped.as_bytes()));
        assert_eq!(status_of(&store, "t"), "installed");
        assert_eq!(first_title(&store.load_installed()).await.unwrap(), "v2");
        assert_eq!(store.update(Some("t")).await.unwrap(), Vec::<String>::new());
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
        assert!(store.update(None).await.unwrap().is_empty());
        let rows = store.list_sources(false).unwrap();
        assert_eq!((rows[0].installed, rows[0].available), (Some(1), Some(2)));
        assert_eq!(rows[0].status, Status::NeedsNewerHon);
        assert_eq!(store.load_installed().len(), 1);
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
        assert!(store.update(None).await.unwrap().is_empty());
        assert_eq!(status_of(&store, "t"), "removed from repo");
        assert!(env.row("t").removed_from_repo);
        let rows = store.list_sources(false).unwrap();
        assert_eq!((rows[0].installed, rows[0].available), (Some(1), None));
        assert_eq!(first_title(&store.load_installed()).await.unwrap(), "v1");
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
        let err = store.update(None).await.unwrap_err();
        let repo_name = &info.location;
        assert_eq!(
            err.to_string(),
            format!(
                "{repo_name}'s signing key changed; if you trust the new key, run \
                 `hon repo remove {repo_name}` and add it again"
            )
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
        let err = store.update(None).await.unwrap_err();
        assert!(format!("{err:#}").contains("lib/gone.lua"), "{err:#}");

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
        assert_eq!(first_title(&store.load_installed()).await.unwrap(), "v1");
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
        let sources = store.load_installed();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id().slug(), "t");
        let err = sources[0].search("x").await.unwrap_err();
        assert!(err.to_string().contains("hash"), "{err}");

        std::fs::remove_file(&file).unwrap();
        let sources = store.load_installed();
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
        let err = first_title(&store.load_installed()).await.unwrap_err();
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
        store.update(None).await.unwrap();
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
        assert!(store.load_installed().is_empty());
    }

    #[tokio::test]
    async fn wrong_manifest_id_becomes_broken() {
        let env = Env::new();
        let store = env.store();
        let mut repo = Repo::new("R");
        repo.lib("util", &util("v1")).source(
            "t",
            1,
            &plugin("mangadex", "http://unused"),
            json!({}),
        );
        repo.publish();
        store.add_repo(repo.path(), false, None).await.unwrap();
        store.install("t").await.unwrap();
        let sources = store.load_installed();
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
        assert_eq!(first_title(&store.load_installed()).await.unwrap(), "v1");

        // An update reads the new commit, not a stale checkout.
        repo.lib("util", &util("v2")).source(
            "t",
            2,
            &format!("--\n{}", plugin("t", &server.uri())),
            json!({}),
        );
        repo.publish();
        commit_all(repo.dir.path());
        assert_eq!(store.update(Some("t")).await.unwrap(), ["t"]);
        assert_eq!(first_title(&store.load_installed()).await.unwrap(), "v2");

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
        assert_eq!(lib_file(folder, "util"), Path::new("/p/3/lib/util.lua"));
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
}
