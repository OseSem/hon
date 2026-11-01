use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use rusqlite::{Connection, params};

use crate::model::{ChapterNo, SourceId};

const MIGRATIONS: &[&str] = &["
    CREATE TABLE series (id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, title TEXT NOT NULL);
    CREATE TABLE chapter_progress (
      series_id INTEGER NOT NULL REFERENCES series(id),
      chapter TEXT NOT NULL,
      read INTEGER NOT NULL DEFAULT 0,
      last_page INTEGER, page_count INTEGER, source TEXT,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (series_id, chapter));
"];

pub struct Db {
    conn: Connection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub read: bool,
    pub last_page: Option<u32>,
    pub page_count: Option<u32>,
    pub source: Option<SourceId>,
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
        Ok(Db { conn })
    }

    pub fn series_id(&self, key: &str, title: &str) -> anyhow::Result<i64> {
        let id = self.conn.query_row(
            "INSERT INTO series (key, title) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET title = excluded.title
             RETURNING id",
            params![key, title],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn progress(&self, series: i64) -> anyhow::Result<HashMap<ChapterNo, Progress>> {
        let mut stmt = self.conn.prepare(
            "SELECT chapter, read, last_page, page_count, source
             FROM chapter_progress WHERE series_id = ?1",
        )?;
        let rows = stmt.query_map([series], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, Option<u32>>(2)?,
                r.get::<_, Option<u32>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (chapter, read, last_page, page_count, source) = row?;
            let ch = ChapterNo::parse(&chapter)
                .ok_or_else(|| anyhow!("invalid chapter number in database: {chapter:?}"))?;
            // A source this binary doesn't know (dropped, or added by a newer hon): no page resume.
            let source = source.and_then(|s| SourceId::from_slug(&s));
            out.insert(
                ch,
                Progress {
                    read,
                    last_page,
                    page_count,
                    source,
                },
            );
        }
        Ok(out)
    }

    pub fn save_page(
        &self,
        series: i64,
        ch: ChapterNo,
        source: SourceId,
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
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// 0-based page to resume at when opening a chapter on `source`.
pub fn resume_page(p: Option<&Progress>, source: SourceId) -> u32 {
    match p {
        Some(p) if !p.read && p.source == Some(source) => p.last_page.unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> ChapterNo {
        ChapterNo::parse(s).unwrap()
    }

    #[test]
    fn series_id_is_stable() {
        let db = Db::open_in_memory().unwrap();
        let a = db.series_id("solo leveling", "Solo Leveling").unwrap();
        assert_eq!(db.series_id("solo leveling", "Solo Leveling").unwrap(), a);
        assert_ne!(db.series_id("one piece", "One Piece").unwrap(), a);
    }

    #[test]
    fn save_page_then_read() {
        let db = Db::open_in_memory().unwrap();
        let s = db.series_id("k", "K").unwrap();
        db.save_page(s, ch("112"), SourceId::MangaKatana, 13, 38)
            .unwrap();
        assert_eq!(
            db.progress(s).unwrap()[&ch("112")],
            Progress {
                read: false,
                last_page: Some(13),
                page_count: Some(38),
                source: Some(SourceId::MangaKatana),
            }
        );
        db.set_read(s, ch("112"), true).unwrap();
        assert!(db.progress(s).unwrap()[&ch("112")].read);
    }

    #[test]
    fn read_status_shared_resume_page_not() {
        let db = Db::open_in_memory().unwrap();
        let s = db.series_id("k", "K").unwrap();
        db.save_page(s, ch("112"), SourceId::MangaKatana, 13, 38)
            .unwrap();
        let progress = db.progress(s).unwrap();
        let p = progress.get(&ch("112"));
        assert_eq!(resume_page(p, SourceId::MangaKatana), 13);
        assert_eq!(resume_page(p, SourceId::MangaDex), 0);
        assert_eq!(resume_page(None, SourceId::MangaKatana), 0);

        db.set_read(s, ch("112"), true).unwrap();
        let progress = db.progress(s).unwrap();
        assert_eq!(
            resume_page(progress.get(&ch("112")), SourceId::MangaKatana),
            0
        );
    }

    #[test]
    fn unread_clears_page() {
        let db = Db::open_in_memory().unwrap();
        let s = db.series_id("k", "K").unwrap();
        db.save_page(s, ch("5"), SourceId::MangaDex, 7, 20).unwrap();
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
        let s = db.series_id("k", "K").unwrap();
        db.set_read(s, ch("10"), true).unwrap();
        db.save_page(s, ch("10.5"), SourceId::MangaDex, 2, 9)
            .unwrap();
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
            let s = db.series_id("k", "K").unwrap();
            db.save_page(s, ch("3"), SourceId::MangaDex, 4, 10).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let s = db.series_id("k", "K").unwrap();
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
        let s = db.series_id("k", "K").unwrap();
        db.conn
            .execute(
                "INSERT INTO chapter_progress (series_id, chapter, source, updated_at)
                 VALUES (?1, '1', 'nosuchsource', 0)",
                [s],
            )
            .unwrap();
        assert_eq!(
            db.progress(s).unwrap()[&ch("1")].source,
            None,
            "a source this hon doesn't know resumes at page 1"
        );

        let t = db.series_id("t", "T").unwrap();
        db.conn
            .execute(
                "INSERT INTO chapter_progress (series_id, chapter, updated_at)
                 VALUES (?1, 'abc', 0)",
                [t],
            )
            .unwrap();
        assert!(db.progress(t).is_err());
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
