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
use crate::plugins::index::{self, Index, IndexSource, lib_name, valid_lib_name};
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

/// What an update did: the ids whose installed version or file changed, and each repo that
/// failed to sync (its location and a one-line error). A failed repo changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub changed: Vec<String>,
    pub failed: Vec<(String, String)>,
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
        for leftover in ["old", "tmp"] {
            remove_dir_if_exists(&self.dir.join(format!("{}.{leftover}", repo.id)))?;
        }
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

    /// Syncs the repo of installed source `id`, or every repo. A repo that fails to sync
    /// goes in [`UpdateReport::failed`] and the others still sync; `Err` is only for what
    /// stops every sync (an id that isn't installed, an unreadable database).
    pub async fn update(&self, id: Option<&str>) -> anyhow::Result<UpdateReport> {
        let mut repos = self.db.plugin_repos()?;
        if let Some(id) = id {
            let row = self
                .installed(id)?
                .with_context(|| format!("{id} is not installed"))?;
            repos.retain(|r| r.id == row.repo_id);
            if repos.is_empty() {
                bail!("{id}'s repo is gone");
            }
        }
        let mut report = UpdateReport::default();
        for repo in &repos {
            match self.sync_repo(repo, None).await {
                Ok(changed) => report.changed.extend(changed),
                Err(e) => report
                    .failed
                    .push((repo.location.clone(), display_text(&format!("{e:#}")))),
            }
        }
        report.changed.sort();
        Ok(report)
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
    /// Must run inside a tokio runtime. `Err` only when the plugin tables can't be read.
    pub fn load_installed(&self) -> anyhow::Result<Vec<Arc<dyn Source>>> {
        let rows = self
            .db
            .plugin_sources()
            .context("reading the installed plugins")?;
        let repos = self.db.plugin_repos().context("reading the plugin repos")?;
        Ok(rows
            .iter()
            .map(|row| match self.load_one(row, &repos) {
                Ok(source) => Arc::new(source) as Arc<dyn Source>,
                Err(e) => Arc::new(BrokenSource::new(
                    SourceId::new(&row.id, &row.name),
                    display_text(&format!("{e:#}")),
                )),
            })
            .collect())
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
        let live = self.repo_dir(repo.id);
        let tmp = self.dir.join(format!("{}.tmp", repo.id));
        let old = self.dir.join(format!("{}.old", repo.id));
        // A sync that died between moving the live folder aside and moving the new one in
        // left only `.old`, which still matches the rows. Restored before anything that can
        // fail (an offline repo, a changed key), so the installed plugins load again.
        if !live.exists() && old.exists() {
            std::fs::rename(&old, &live)
                .with_context(|| format!("restoring {} from {}", live.display(), old.display()))?;
        }

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

        remove_dir_if_exists(&tmp)?;
        let index_json = serde_json::to_string(&index)?;
        let staged = match self.stage(repo, &loc, &index, added, &live, &tmp).await {
            Ok(staged) => staged,
            Err(e) => return Err(with_undo(e, remove_dir_if_exists(&tmp))),
        };

        remove_dir_if_exists(&old)?;
        let had_live = live.exists();
        let move_old_back = || -> anyhow::Result<()> {
            if had_live {
                std::fs::rename(&old, &live)
                    .with_context(|| format!("moving {} back", old.display()))?;
            }
            Ok(())
        };
        if had_live && let Err(e) = std::fs::rename(&live, &old) {
            let e = anyhow!(e).context(format!("moving {} aside", live.display()));
            return Err(with_undo(e, remove_dir_if_exists(&tmp)));
        }
        if let Err(e) = std::fs::rename(&tmp, &live) {
            // `live` isn't ours here (whatever is there now came from someone else, such as
            // another hon restoring it), so only `.old` moves back.
            let e = anyhow!(e).context(format!("moving {} into place", tmp.display()));
            let undo = move_old_back().and_then(|()| remove_dir_if_exists(&tmp));
            return Err(with_undo(e, undo));
        }
        // From here `live` is the folder this sync staged.
        let put_back = || remove_dir_if_exists(&live).and_then(|()| move_old_back());
        if let Err(e) = self.db.sync_plugin_repo(
            repo.id,
            &index.name,
            &index_json,
            &staged.rows,
            &staged.libs,
        ) {
            return Err(with_undo(e, put_back()));
        }
        // The sync is done; a leftover `.old` is removed by the next one.
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
        let mut kept_any = false;
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
                    kept_any = true;
                    copy_if_exists(&source_file(live, &row.id)?, &source_file(tmp, &row.id)?)?;
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
            self.download(repo, loc, &lib.file, &lib.sha256, &lib_file(tmp, name)?)
                .await?;
            staged.libs.push((name.to_string(), lib.sha256.clone()));
        }
        // A kept source may need a lib the index dropped, so those stay too. A lib that
        // changed under the same name is the new one for kept sources as well (a v1 limit:
        // hon keeps one copy of each lib per repo).
        if kept_any {
            let mut old_libs: Vec<_> = self.db.plugin_libs(repo.id)?.into_iter().collect();
            old_libs.sort();
            for (name, sha256) in old_libs {
                if staged.libs.iter().any(|(n, _)| *n == name) {
                    continue;
                }
                if copy_if_exists(&lib_file(live, &name)?, &lib_file(tmp, &name)?)? {
                    staged.libs.push((name, sha256));
                }
            }
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

/// `<folder>/lib/<name>.lua`, for a valid lib name only.
fn lib_file(folder: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if !valid_lib_name(name) {
        bail!("{name:?} is not a valid lib name");
    }
    Ok(folder.join("lib").join(format!("{name}.lua")))
}

/// Copies `from` to `to`; false when `from` doesn't exist.
fn copy_if_exists(from: &Path, to: &Path) -> anyhow::Result<bool> {
    match std::fs::copy(from, to) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("copying {}", from.display())),
    }
}

/// `err`, with what went wrong undoing its step, if anything did.
fn with_undo(err: anyhow::Error, undo: anyhow::Result<()>) -> anyhow::Error {
    match undo {
        Ok(()) => err,
        Err(u) => anyhow!("{err:#}; undoing it also failed: {u:#}"),
    }
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
mod tests;
