//! `hon repo …` and `hon source …`: the repos sources come from, installed sources, and the
//! plugin author's tools (`test`, `index`). Text from plugins and indexes comes back from
//! hon-core already cleaned for the terminal.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};

use crate::cli::{RepoCommand, SourceCommand, TestCall};
use crate::config::Paths;
use hon_core::db::Db;
use hon_core::model::{ChapterNo, ChapterRef, MangaRef};
use hon_core::plugins::index::{Index, IndexLib, IndexSource, valid_lib_name};
use hon_core::plugins::runtime::{API_VERSION, LibSource, display_text, sha256_hex};
use hon_core::plugins::source::{LoadOpts, PluginSource};
use hon_core::plugins::store::{SourceRow, Store};
use hon_core::sources::{Source, http_client};

pub async fn repo(paths: &Paths, command: RepoCommand, out: &mut impl Write) -> anyhow::Result<()> {
    let db = Db::open(&paths.db_file)?;
    let store = store(&db, paths);
    match command {
        RepoCommand::Add { location, git, key } => {
            let info = store.add_repo(&location, git, key.as_deref()).await?;
            writeln!(out, "Added {} ({})", info.name, info.location)?;
            writeln!(out, "Signing key: {}", info.pubkey)?;
        }
        RepoCommand::List => {
            let rows: Vec<_> = store
                .list_repos()?
                .into_iter()
                .map(|r| vec![r.name, r.location])
                .collect();
            if rows.is_empty() {
                writeln!(out, "no source repos added")?;
            }
            write_table(rows, out)?;
        }
        RepoCommand::Remove { repo } => {
            store.remove_repo(&repo)?;
            writeln!(out, "Removed {repo}")?;
        }
    }
    Ok(())
}

pub async fn source(
    paths: &Paths,
    command: SourceCommand,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    match command {
        SourceCommand::List { nsfw } => {
            let db = Db::open(&paths.db_file)?;
            let rows: Vec<_> = store(&db, paths)
                .list_sources(nsfw)?
                .iter()
                .map(source_row)
                .collect();
            if rows.is_empty() {
                writeln!(
                    out,
                    "no sources available; add a repo with `hon repo add <url>`"
                )?;
            }
            write_table(rows, out)
        }
        SourceCommand::Install { id } => {
            let db = Db::open(&paths.db_file)?;
            install(&store(&db, paths), &id, out).await
        }
        SourceCommand::Update { id } => {
            let db = Db::open(&paths.db_file)?;
            update(&store(&db, paths), id.as_deref(), out).await
        }
        SourceCommand::Remove { id } => {
            let db = Db::open(&paths.db_file)?;
            store(&db, paths).remove(&id)?;
            writeln!(out, "Removed {id}")?;
            Ok(())
        }
        // The plugin author's tools work on files and need no database.
        SourceCommand::Test {
            file,
            lib_dir,
            call,
        } => test(&file, lib_dir, &call, out).await,
        SourceCommand::Index { repo_dir } => index(&repo_dir, out),
    }
}

fn store<'a>(db: &'a Db, paths: &Paths) -> Store<'a> {
    Store::new(db, &paths.plugins_dir, http_client())
}

/// Installs `id`, first saying what to do when no added repo offers it.
async fn install(store: &Store<'_>, id: &str, out: &mut impl Write) -> anyhow::Result<()> {
    let offered = store
        .list_sources(true)?
        .iter()
        .any(|r| r.id == id && r.available.is_some());
    if !offered {
        if store.list_repos()?.is_empty() {
            bail!("no source repos added; add one with `hon repo add <url>`");
        }
        bail!("no added repo offers `{id}`; run `hon source update` to refresh the repo lists");
    }
    store.install(id).await?;
    writeln!(out, "Installed {id}")?;
    Ok(())
}

/// Prints what changed and, on stderr, each repo that failed; errors if any failed.
async fn update(store: &Store<'_>, id: Option<&str>, out: &mut impl Write) -> anyhow::Result<()> {
    let report = store.update(id).await?;
    for id in &report.changed {
        writeln!(out, "updated {id}")?;
    }
    for (location, error) in &report.failed {
        eprintln!("warning: {location}: {error}");
    }
    match report.failed.len() {
        0 if report.changed.is_empty() => writeln!(out, "everything is up to date")?,
        0 => {}
        1 => bail!("1 repo couldn't be updated"),
        n => bail!("{n} repos couldn't be updated"),
    }
    Ok(())
}

fn source_row(row: &SourceRow) -> Vec<String> {
    let version = match (row.installed, row.available) {
        (Some(installed), Some(available)) if installed != available => {
            format!("v{installed} → v{available}")
        }
        (Some(v), _) | (None, Some(v)) => format!("v{v}"),
        (None, None) => String::new(),
    };
    vec![
        row.id.clone(),
        row.name.clone(),
        row.lang.clone(),
        version,
        row.repo.clone(),
        row.status.to_string(),
    ]
}

/// Rows in columns two spaces apart, the last one unpadded.
fn write_table(rows: Vec<Vec<String>>, out: &mut impl Write) -> anyhow::Result<()> {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in &rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == row.len() {
                line += cell;
            } else {
                line += &format!("{cell:<width$}  ", width = widths[i]);
            }
        }
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// `hon source test`: loads `file` as a plugin and prints what one function returns.
async fn test(
    file: &Path,
    lib_dir: Option<PathBuf>,
    call: &TestCall,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let code =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let chunk = chunk_name(file);
    let lib_dir = lib_dir.unwrap_or_else(|| {
        file.parent()
            .unwrap_or(Path::new(""))
            .join("..")
            .join("lib")
    });
    let opts = LoadOpts {
        log: Arc::new(|id: &str, msg: &str| {
            eprintln!("{}: {}", display_text(id), display_text(msg));
        }),
        ..LoadOpts::default()
    };
    let source = PluginSource::load(&code, &chunk, LibSource::Dir(lib_dir), opts)?;
    if let Some(query) = &call.search {
        for m in source.search(query).await? {
            writeln!(out, "{}  {}  {}", m.id, m.title, m.url)?;
        }
    } else if let Some(id) = &call.chapters {
        let manga = MangaRef {
            source: source.id(),
            id: id.clone(),
            title: String::new(),
            alt_titles: Vec::new(),
            url: String::new(),
        };
        for c in source.chapters(&manga).await? {
            match &c.title {
                Some(title) => writeln!(out, "{}  {}  {title}", c.number, c.id)?,
                None => writeln!(out, "{}  {}", c.number, c.id)?,
            }
        }
    } else if let Some(id) = &call.pages {
        let chapter = ChapterRef {
            source: source.id(),
            id: id.clone(),
            number: ChapterNo::parse("0").expect("0 is a chapter number"),
            title: None,
            published: None,
        };
        for page in source.pages(&chapter).await? {
            writeln!(out, "{}", page.url)?;
        }
    }
    Ok(())
}

/// The file's name, for error messages and the log.
fn chunk_name(file: &Path) -> String {
    let name = file.file_name().unwrap_or(file.as_os_str());
    display_text(&name.to_string_lossy())
}

/// `hon source index`: writes `<repo_dir>/index.json` for every `sources/*.lua` and
/// `lib/*.lua`. A source's `libs` are the modules its file requires while it loads.
fn index(repo_dir: &Path, out: &mut impl Write) -> anyhow::Result<()> {
    let lib_dir = repo_dir.join("lib");
    let mut sources: Vec<IndexSource> = Vec::new();
    for path in lua_files(&repo_dir.join("sources"))? {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("{} is not a UTF-8 file name", path.display()))?;
        let code = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let plugin = PluginSource::load(
            &code,
            &chunk_name(&path),
            LibSource::Dir(lib_dir.clone()),
            LoadOpts::default(),
        )?;
        let m = plugin.manifest();
        if let Some(other) = sources.iter().find(|s| s.id == m.id) {
            bail!("{file_name} and {} both have the id `{}`", other.file, m.id);
        }
        sources.push(IndexSource {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version,
            lang: m.lang.clone(),
            nsfw: m.nsfw,
            api: m.api,
            file: format!("sources/{file_name}"),
            sha256: sha256_hex(code.as_bytes()),
            libs: plugin
                .required_modules()
                .iter()
                .map(|name| format!("lib/{name}.lua"))
                .collect(),
        });
    }
    if sources.is_empty() {
        bail!("{} has no .lua files", repo_dir.join("sources").display());
    }

    let mut libs = Vec::new();
    if lib_dir.is_dir() {
        for path in lua_files(&lib_dir)? {
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // `require` can't name any other file.
            if !valid_lib_name(name) {
                continue;
            }
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            libs.push(IndexLib {
                file: format!("lib/{name}.lua"),
                sha256: sha256_hex(&bytes),
            });
        }
    }

    let path = repo_dir.join("index.json");
    let index = Index {
        name: index_name(repo_dir, &path)?,
        api: API_VERSION,
        sources,
        libs,
    };
    let mut json = serde_json::to_vec_pretty(&index)?;
    json.push(b'\n');
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    let shown = path.display();
    writeln!(out, "{shown}")?;
    writeln!(out, "sign it: minisign -Sm {shown}")?;
    writeln!(
        out,
        "note: a source's libs are the modules its file requires at the top level; \
         require libs at the top of the file, not inside functions"
    )?;
    Ok(())
}

/// The `name` the index at `path` has, else the repo folder's name.
fn index_name(repo_dir: &Path, path: &Path) -> anyhow::Result<String> {
    let existing = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|index| index.get("name")?.as_str().map(str::to_string))
        .filter(|name| !name.trim().is_empty());
    if let Some(name) = existing {
        return Ok(name);
    }
    let dir = repo_dir
        .canonicalize()
        .with_context(|| format!("resolving {}", repo_dir.display()))?;
    Ok(dir
        .file_name()
        .map_or("sources".into(), |n| n.to_string_lossy().into_owned()))
}

/// The `.lua` files in `dir`, sorted.
fn lua_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        if path.extension().is_some_and(|e| e == "lua") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}
