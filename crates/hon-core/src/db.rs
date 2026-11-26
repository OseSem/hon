use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{ChapterNo, SourceId};

const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE series (id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, title TEXT NOT NULL);
    CREATE TABLE chapter_progress (
      series_id INTEGER NOT NULL REFERENCES series(id),
      chapter TEXT NOT NULL,
      read INTEGER NOT NULL DEFAULT 0,
      last_page INTEGER, page_count INTEGER, source TEXT,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (series_id, chapter));
",
    // Every normalized title a series has been found under, so sources that title it
    // differently still find the same series.
    "
    CREATE TABLE series_alias (
      key TEXT PRIMARY KEY,
      series_id INTEGER NOT NULL REFERENCES series(id));
    INSERT INTO series_alias (key, series_id) SELECT key, id FROM series;
",
];

/// Source plugin repos, the plugins installed from them and the libs they share. Not a
/// migration: hon 0.1.0 refuses a database whose `user_version` is past its own 2, and adding
/// tables leaves the database readable to it, so going back stays possible.
const PLUGIN_TABLES: &str = "
    CREATE TABLE IF NOT EXISTS plugin_repo (
      id INTEGER PRIMARY KEY, location TEXT NOT NULL UNIQUE, kind TEXT NOT NULL,
      pubkey TEXT NOT NULL, name TEXT NOT NULL, index_json TEXT NOT NULL, fetched_at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS plugin_source (
      id TEXT PRIMARY KEY, repo_id INTEGER NOT NULL REFERENCES plugin_repo(id) ON DELETE CASCADE,
      name TEXT NOT NULL, version INTEGER NOT NULL, lang TEXT NOT NULL, nsfw INTEGER NOT NULL,
      file TEXT NOT NULL, sha256 TEXT NOT NULL, removed_from_repo INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE IF NOT EXISTS plugin_lib (
      repo_id INTEGER NOT NULL REFERENCES plugin_repo(id) ON DELETE CASCADE,
      name TEXT NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY (repo_id, name));
";

pub struct Db {
    conn: Connection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub id: i64,
    /// The title the series was first opened under; download folders are named after it.
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub read: bool,
    pub last_page: Option<u32>,
    pub page_count: Option<u32>,
    /// The slug of the source the page was saved on.
    pub source: Option<String>,
    /// Unix seconds of the last change.
    pub updated_at: i64,
}

/// A source plugin repo the user added.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginRepo {
    pub id: i64,
    /// The index URL, the absolute folder path, or `git:<url>`.
    pub location: String,
    /// `https`, `dir` or `git`.
    pub kind: String,
    /// The pinned minisign key, base64.
    pub pubkey: String,
    pub name: String,
    /// The last verified index, re-serialized.
    pub index_json: String,
}

/// An installed source plugin.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginRow {
    pub id: String,
    pub repo_id: i64,
    pub name: String,
    pub version: u32,
    pub lang: String,
    pub nsfw: bool,
    /// Its path in the repo, as the index gave it.
    pub file: String,
    pub sha256: String,
    pub removed_from_repo: bool,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Db::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Db> {
        Db::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> anyhow::Result<Db> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version as usize > MIGRATIONS.len() {
            bail!("this database was created by a newer version of hon");
        }
        for (sql, v) in MIGRATIONS.iter().zip(1u32..).skip(version as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", v)?;
            tx.commit()?;
        }
        conn.execute_batch(PLUGIN_TABLES)?;
        Ok(Db { conn })
    }

    /// The series any of `keys` names, learning the keys it didn't have yet; else a new
    /// series titled `title` under all of them. `keys[0]` is the primary key.
    pub fn series(&self, keys: &[String], title: &str) -> anyhow::Result<Series> {
        let Some(primary) = keys.first() else {
            bail!("a series needs at least one key");
        };
        let tx = self.conn.unchecked_transaction()?;
        let mut found = None;
        for key in keys {
            found = tx
                .query_row(
                    "SELECT series_id FROM series_alias WHERE key = ?1",
                    [key],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?;
            if found.is_some() {
                break;
            }
        }
        let id = match found {
            Some(id) => id,
            None => tx.query_row(
                "INSERT INTO series (key, title) VALUES (?1, ?2) RETURNING id",
                params![primary, title],
                |r| r.get(0),
            )?,
        };
        for key in keys {
            tx.execute(
                "INSERT OR IGNORE INTO series_alias (key, series_id) VALUES (?1, ?2)",
                params![key, id],
            )?;
        }
        let title = tx.query_row("SELECT title FROM series WHERE id = ?1", [id], |r| r.get(0))?;
        tx.commit()?;
        Ok(Series { id, title })
    }

    pub fn progress(&self, series: i64) -> anyhow::Result<HashMap<ChapterNo, Progress>> {
        let mut stmt = self.conn.prepare(
            "SELECT chapter, read, last_page, page_count, source, updated_at
             FROM chapter_progress WHERE series_id = ?1",
        )?;
        let rows = stmt.query_map([series], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, Option<u32>>(2)?,
                r.get::<_, Option<u32>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (chapter, read, last_page, page_count, source, updated_at) = row?;
            let ch = ChapterNo::parse(&chapter)
                .ok_or_else(|| anyhow!("invalid chapter number in database: {chapter:?}"))?;
            out.insert(
                ch,
                Progress {
                    read,
                    last_page,
                    page_count,
                    source,
                    updated_at,
                },
            );
        }
        Ok(out)
    }

    pub fn save_page(
        &self,
        series: i64,
        ch: ChapterNo,
        source: &SourceId,
        page: u32,
        page_count: u32,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO chapter_progress (series_id, chapter, last_page, page_count, source, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (series_id, chapter) DO UPDATE SET
               last_page = excluded.last_page,
               page_count = excluded.page_count,
               source = excluded.source,
               updated_at = excluded.updated_at",
            params![series, ch.to_string(), page, page_count, source.slug(), now()],
        )?;
        Ok(())
    }

    pub fn set_read(&self, series: i64, ch: ChapterNo, read: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO chapter_progress (series_id, chapter, read, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (series_id, chapter) DO UPDATE SET
               read = excluded.read,
               last_page = CASE WHEN excluded.read THEN last_page ELSE NULL END,
               updated_at = excluded.updated_at",
            params![series, ch.to_string(), read, now()],
        )?;
        Ok(())
    }

    /// Every plugin repo, in the order they were added.
    pub fn plugin_repos(&self) -> anyhow::Result<Vec<PluginRepo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, location, kind, pubkey, name, index_json FROM plugin_repo ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PluginRepo {
                id: r.get(0)?,
                location: r.get(1)?,
                kind: r.get(2)?,
                pubkey: r.get(3)?,
                name: r.get(4)?,
                index_json: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Adds a repo and returns its id.
    pub fn add_plugin_repo(
        &self,
        location: &str,
        kind: &str,
        pubkey: &str,
        name: &str,
        index_json: &str,
    ) -> anyhow::Result<i64> {
        Ok(self.conn.query_row(
            "INSERT INTO plugin_repo (location, kind, pubkey, name, index_json, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
            params![location, kind, pubkey, name, index_json, now()],
            |r| r.get(0),
        )?)
    }

    /// Removes a repo with its installed plugins and libs.
    pub fn remove_plugin_repo(&self, id: i64) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM plugin_repo WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Every installed plugin, sorted by id.
    pub fn plugin_sources(&self) -> anyhow::Result<Vec<PluginRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, repo_id, name, version, lang, nsfw, file, sha256, removed_from_repo
             FROM plugin_source ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PluginRow {
                id: r.get(0)?,
                repo_id: r.get(1)?,
                name: r.get(2)?,
                version: r.get(3)?,
                lang: r.get(4)?,
                nsfw: r.get(5)?,
                file: r.get(6)?,
                sha256: r.get(7)?,
                removed_from_repo: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A repo's libs: name to SHA-256.
    pub fn plugin_libs(&self, repo_id: i64) -> anyhow::Result<HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, sha256 FROM plugin_lib WHERE repo_id = ?1")?;
        let rows = stmt.query_map([repo_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forgets an installed plugin; false when it wasn't installed.
    pub fn remove_plugin_source(&self, id: &str) -> anyhow::Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM plugin_source WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// Records a sync of repo `repo_id` in one transaction: its name and index, its installed
    /// plugins (`sources`, inserted or updated) and its libs (replaced).
    pub fn sync_plugin_repo(
        &self,
        repo_id: i64,
        name: &str,
        index_json: &str,
        sources: &[PluginRow],
        libs: &[(String, String)],
    ) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE plugin_repo SET name = ?2, index_json = ?3, fetched_at = ?4 WHERE id = ?1",
            params![repo_id, name, index_json, now()],
        )?;
        for s in sources {
            // The WHERE keeps a plugin from moving between repos: it updates nothing then.
            let n = tx.execute(
                "INSERT INTO plugin_source
                   (id, repo_id, name, version, lang, nsfw, file, sha256, removed_from_repo)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (id) DO UPDATE SET
                   name = excluded.name, version = excluded.version, lang = excluded.lang,
                   nsfw = excluded.nsfw, file = excluded.file, sha256 = excluded.sha256,
                   removed_from_repo = excluded.removed_from_repo
                 WHERE plugin_source.repo_id = excluded.repo_id",
                params![
                    s.id,
                    repo_id,
                    s.name,
                    s.version,
                    s.lang,
                    s.nsfw,
                    s.file,
                    s.sha256,
                    s.removed_from_repo
                ],
            )?;
            if n == 0 {
                bail!("{} is installed from another repo", s.id);
            }
        }
        tx.execute("DELETE FROM plugin_lib WHERE repo_id = ?1", [repo_id])?;
        for (lib, sha256) in libs {
            tx.execute(
                "INSERT INTO plugin_lib (repo_id, name, sha256) VALUES (?1, ?2, ?3)",
                params![repo_id, lib, sha256],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// Where reading picks up among `chapters` (ascending, distinct): the most recently
/// touched chapter if it's unread, else the next one (or the last, when there's none
/// after it); the first chapter when nothing was touched yet.
pub fn resume_chapter(
    chapters: &[ChapterNo],
    progress: &HashMap<ChapterNo, Progress>,
) -> Option<ChapterNo> {
    let Some((&last, p)) = progress.iter().max_by_key(|(n, p)| (p.updated_at, **n)) else {
        return chapters.first().copied();
    };
    if !p.read && chapters.contains(&last) {
        return Some(last);
    }
    chapters
        .iter()
        .copied()
        .find(|&n| n > last)
        .or(chapters.last().copied())
}

/// 0-based page to resume at when opening a chapter on `source`.
pub fn resume_page(p: Option<&Progress>, source: &SourceId) -> u32 {
    match p {
        Some(p) if !p.read && p.source.as_deref() == Some(source.slug()) => {
            p.last_page.unwrap_or(0)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_ids::*;

    fn ch(s: &str) -> ChapterNo {
        ChapterNo::parse(s).unwrap()
    }

    fn keys(ks: &[&str]) -> Vec<String> {
        ks.iter().map(|k| k.to_string()).collect()
    }

    fn sid(db: &Db, key: &str) -> i64 {
        db.series(&keys(&[key]), &key.to_uppercase()).unwrap().id
    }

    #[test]
    fn series_id_is_stable() {
        let db = Db::open_in_memory().unwrap();
        let a = db
            .series(&keys(&["solo leveling"]), "Solo Leveling")
            .unwrap();
        assert_eq!(
            db.series(&keys(&["solo leveling"]), "Solo Leveling")
                .unwrap(),
            a
        );
        assert_ne!(sid(&db, "one piece"), a.id);
    }

    #[test]
    fn series_found_by_any_alias_keeps_first_title() {
        let db = Db::open_in_memory().unwrap();
        let first = db
            .series(
                &keys(&["na honjaman level up", "solo leveling"]),
                "Na Honjaman Level-Up",
            )
            .unwrap();
        let again = db
            .series(
                &keys(&["solo leveling", "only i level up"]),
                "Solo Leveling",
            )
            .unwrap();
        assert_eq!(again, first, "same id, title not overwritten");
        assert_eq!(
            db.series(&keys(&["only i level up"]), "Only I Level Up")
                .unwrap(),
            first,
            "the new alias was learned"
        );
        assert!(db.series(&[], "X").is_err());
    }

    #[test]
    fn migration_from_v1_keeps_data_and_adds_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hon.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO series (key, title) VALUES ('solo leveling', 'Solo Leveling')",
            [],
        )
        .unwrap();
        let old_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chapter_progress (series_id, chapter, read, updated_at)
             VALUES (?1, '3', 1, 0)",
            [old_id],
        )
        .unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        let version: u32 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
        let s = db
            .series(
                &keys(&["na honjaman level up", "solo leveling"]),
                "Na Honjaman Level-Up",
            )
            .unwrap();
        assert_eq!(
            s,
            Series {
                id: old_id,
                title: "Solo Leveling".into()
            }
        );
        assert!(db.progress(s.id).unwrap()[&ch("3")].read);
    }

    #[test]
    fn save_page_then_read() {
        let db = Db::open_in_memory().unwrap();
        let s = sid(&db, "k");
        let before = now();
        db.save_page(s, ch("112"), &beta(), 13, 38).unwrap();
        let p = db.progress(s).unwrap()[&ch("112")].clone();
        assert!((before..=now()).contains(&p.updated_at), "{p:?}");
        assert_eq!(
            p,
            Progress {
                read: false,
                last_page: Some(13),
                page_count: Some(38),
                source: Some("beta".into()),
                updated_at: p.updated_at,
            }
        );
        db.set_read(s, ch("112"), true).unwrap();
        assert!(db.progress(s).unwrap()[&ch("112")].read);
    }

    #[test]
    fn read_status_shared_resume_page_not() {
        let db = Db::open_in_memory().unwrap();
        let s = sid(&db, "k");
        db.save_page(s, ch("112"), &beta(), 13, 38).unwrap();
        let progress = db.progress(s).unwrap();
        let p = progress.get(&ch("112"));
        assert_eq!(resume_page(p, &beta()), 13);
        assert_eq!(resume_page(p, &alpha()), 0);
        assert_eq!(resume_page(None, &beta()), 0);

        db.set_read(s, ch("112"), true).unwrap();
        let progress = db.progress(s).unwrap();
        assert_eq!(resume_page(progress.get(&ch("112")), &beta()), 0);
    }

    #[test]
    fn unread_clears_page() {
        let db = Db::open_in_memory().unwrap();
        let s = sid(&db, "k");
        db.save_page(s, ch("5"), &alpha(), 7, 20).unwrap();
        db.set_read(s, ch("5"), true).unwrap();
        db.set_read(s, ch("5"), false).unwrap();
        let p = &db.progress(s).unwrap()[&ch("5")];
        assert!(!p.read);
        assert_eq!(p.last_page, None);
        assert_eq!(p.page_count, Some(20));
    }

    #[test]
    fn fractional_chapter_keys() {
        let db = Db::open_in_memory().unwrap();
        let s = sid(&db, "k");
        db.set_read(s, ch("10"), true).unwrap();
        db.save_page(s, ch("10.5"), &alpha(), 2, 9).unwrap();
        let progress = db.progress(s).unwrap();
        assert_eq!(progress.len(), 2);
        assert!(progress[&ch("10")].read);
        assert_eq!(progress[&ch("10")].last_page, None);
        assert!(!progress[&ch("10.5")].read);
        assert_eq!(progress[&ch("10.5")].last_page, Some(2));
    }

    #[test]
    fn reopen_file_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("hon.db");
        {
            let db = Db::open(&path).unwrap();
            let s = sid(&db, "k");
            db.save_page(s, ch("3"), &alpha(), 4, 10).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let s = sid(&db, "k");
        assert_eq!(db.progress(s).unwrap()[&ch("3")].last_page, Some(4));
        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn corrupt_chapter_is_error_unknown_source_is_not() {
        let db = Db::open_in_memory().unwrap();
        let s = sid(&db, "k");
        db.conn
            .execute(
                "INSERT INTO chapter_progress (series_id, chapter, last_page, source, updated_at)
                 VALUES (?1, '1', 5, 'nosuchsource', 0)",
                [s],
            )
            .unwrap();
        let progress = db.progress(s).unwrap();
        let p = progress.get(&ch("1"));
        assert_eq!(p.unwrap().source.as_deref(), Some("nosuchsource"));
        assert_eq!(
            resume_page(p, &alpha()),
            0,
            "a source this hon doesn't know resumes at page 1"
        );
        let unknown = SourceId::new("nosuchsource", "No Such Source");
        assert_eq!(resume_page(p, &unknown), 5, "its own page, once installed");

        let t = sid(&db, "t");
        db.conn
            .execute(
                "INSERT INTO chapter_progress (series_id, chapter, updated_at)
                 VALUES (?1, 'abc', 0)",
                [t],
            )
            .unwrap();
        assert!(db.progress(t).is_err());
    }

    fn touched(entries: &[(&str, bool, i64)]) -> HashMap<ChapterNo, Progress> {
        entries
            .iter()
            .map(|&(n, read, updated_at)| {
                let p = Progress {
                    read,
                    last_page: (!read).then_some(4),
                    page_count: None,
                    source: None,
                    updated_at,
                };
                (ch(n), p)
            })
            .collect()
    }

    #[test]
    fn resume_chapter_follows_the_last_touched() {
        let chapters: Vec<ChapterNo> = ["0", "1", "2", "3", "3.5", "4"].map(ch).to_vec();
        let resume = |entries| resume_chapter(&chapters, &touched(entries));
        assert_eq!(resume(&[]), Some(ch("0")), "nothing read: the first");
        assert_eq!(
            resume(&[("1", false, 1), ("2", true, 9), ("3", true, 9)]),
            Some(ch("3.5")),
            "after the newest read one, not the old stray page"
        );
        assert_eq!(
            resume(&[("1", true, 1), ("3", false, 5)]),
            Some(ch("3")),
            "the newest is in progress"
        );
        assert_eq!(
            resume(&[("3", true, 5), ("3.5", true, 5)]),
            Some(ch("4")),
            "same second: the higher chapter"
        );
        assert_eq!(
            resume(&[("4", true, 5)]),
            Some(ch("4")),
            "all done: the last"
        );
        assert_eq!(
            resume(&[("2.5", true, 5)]),
            Some(ch("3")),
            "a chapter no source has any more"
        );
        assert_eq!(resume_chapter(&[], &touched(&[("1", true, 1)])), None);
    }

    fn plugin_row(id: &str, repo_id: i64, version: u32) -> PluginRow {
        PluginRow {
            id: id.into(),
            repo_id,
            name: id.to_uppercase(),
            version,
            lang: "en".into(),
            nsfw: false,
            file: format!("sources/{id}.lua"),
            sha256: "a".repeat(64),
            removed_from_repo: false,
        }
    }

    #[test]
    fn plugin_sync_upserts_and_cascades() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add_plugin_repo("/a", "dir", "k", "A", "{}").unwrap();
        let b = db.add_plugin_repo("/b", "dir", "k", "B", "{}").unwrap();
        assert!(db.add_plugin_repo("/a", "dir", "k", "A", "{}").is_err());

        let libs = [("util".to_string(), "b".repeat(64))];
        db.sync_plugin_repo(a, "A2", "{\"x\":1}", &[plugin_row("t", a, 1)], &libs)
            .unwrap();
        db.sync_plugin_repo(a, "A2", "{}", &[plugin_row("t", a, 2)], &[])
            .unwrap();
        assert_eq!(db.plugin_sources().unwrap(), [plugin_row("t", a, 2)]);
        assert!(db.plugin_libs(a).unwrap().is_empty());
        assert_eq!(db.plugin_repos().unwrap()[0].name, "A2");

        // A plugin can't move to another repo, and the failed sync changes nothing.
        let err = db
            .sync_plugin_repo(b, "B2", "{}", &[plugin_row("t", b, 9)], &libs)
            .unwrap_err();
        assert_eq!(err.to_string(), "t is installed from another repo");
        assert_eq!(db.plugin_sources().unwrap(), [plugin_row("t", a, 2)]);
        assert_eq!(db.plugin_repos().unwrap()[1].name, "B");
        assert!(db.plugin_libs(b).unwrap().is_empty());

        db.sync_plugin_repo(b, "B", "{}", &[], &libs).unwrap();
        db.remove_plugin_repo(a).unwrap();
        assert!(db.plugin_sources().unwrap().is_empty());
        assert_eq!(db.plugin_libs(b).unwrap().len(), 1);
        assert!(!db.remove_plugin_source("t").unwrap());
    }

    /// hon 0.1.0 writes version 2 and refuses anything newer, so the plugin tables don't
    /// raise it: going back to 0.1.0 must keep working.
    #[test]
    fn plugin_tables_keep_the_v2_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hon.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch(MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        drop(conn);

        for _ in 0..2 {
            let db = Db::open(&path).unwrap();
            let version: u32 = db
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 2);
            assert!(db.plugin_sources().unwrap().is_empty());
            assert!(db.plugin_repos().unwrap().is_empty());
            assert!(db.plugin_libs(1).unwrap().is_empty());
        }
    }

    #[test]
    fn newer_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hon.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();
        drop(conn);
        let err = Db::open(&path).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            "this database was created by a newer version of hon"
        );
    }
}
