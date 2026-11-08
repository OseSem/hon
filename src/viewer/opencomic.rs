//! What hon knows about OpenComic: how to spot it in the `viewer` command, where it keeps
//! its files, and how to read its reading progress. Reads only; never writes its files.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde_json::Value;

use crate::model::ChapterNo;

/// Per-comic reading progress, in OpenComic's storage folder.
pub const PROGRESS_FILE: &str = "readingProgress.json";
/// OpenComic's settings, in the same folder.
pub const CONFIG_FILE: &str = "config.json";

/// Whether the `viewer` command runs OpenComic (`flatpak run app.opencomic.OpenComic`,
/// `opencomic`, `open -a OpenComic`, …).
pub fn is_opencomic(viewer: Option<&str>) -> bool {
    viewer.is_some_and(|v| v.to_lowercase().contains("opencomic"))
}

/// Where OpenComic may keep its storage folder, most likely first: the Flatpak's, then
/// Electron's usual place under the OS config folder (`~/.config`, `~/Library/Application
/// Support`, `%APPDATA%`).
pub fn storage_candidates(home: &Path, config_dir: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".var/app/app.opencomic.OpenComic/config/OpenComic/storage"),
        config_dir.join("OpenComic").join("storage"),
    ]
}

/// One chapter's progress as OpenComic saved it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub chapter: ChapterNo,
    /// 1-based page you were on.
    pub page: u32,
    pub pages: u32,
    pub completed: bool,
    /// When it was saved, in Unix milliseconds.
    pub last_reading_ms: i64,
}

/// The entries of `readingProgress.json` whose key `chapter_of` recognizes as a chapter.
/// Keys it doesn't (other series, the series folder, OpenComic's samples) and entries
/// missing a field are left out.
pub fn entries(
    json: &str,
    chapter_of: impl Fn(&Path) -> Option<ChapterNo>,
) -> anyhow::Result<Vec<Entry>> {
    let all: Value = serde_json::from_str(json).context("reading OpenComic's progress")?;
    let all = all
        .as_object()
        .ok_or_else(|| anyhow!("OpenComic's progress isn't a JSON object"))?;
    Ok(all
        .iter()
        .filter_map(|(key, v)| {
            let number = |field: &str| v.get(field)?.as_u64()?.try_into().ok();
            Some(Entry {
                chapter: chapter_of(Path::new(key))?,
                page: number("page")?,
                pages: number("pages")?,
                completed: v.get("completed")?.as_bool()?,
                last_reading_ms: v.get("lastReading")?.as_i64()?,
            })
        })
        .collect())
}

/// The chapter being read: the one saved last, or the one after it among `chapters`
/// (ascending) when it was finished. OpenComic saves a chapter as you leave it, and the
/// one you move on to only minutes later, so a finished newest entry means you moved on.
pub fn current_chapter(entries: &[Entry], chapters: &[ChapterNo]) -> Option<ChapterNo> {
    let newest = entries
        .iter()
        .max_by_key(|e| (e.last_reading_ms, e.chapter))?;
    if !newest.completed {
        return Some(newest.chapter);
    }
    let next = chapters.iter().copied().find(|&n| n > newest.chapter);
    Some(next.unwrap_or(newest.chapter))
}

/// Whether OpenComic's `config.json` keeps its default chapter order, which puts `Ch 9.5`
/// before `Ch 9`. Its "numeric" order doesn't. Unreadable settings count as fine.
pub fn sorts_half_chapters_wrong(config_json: &str) -> bool {
    let Ok(config) = serde_json::from_str::<Value>(config_json) else {
        return false;
    };
    config.get("sortReading").and_then(Value::as_str) != Some("numeric")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::model::ChapterNo;

    fn ch(s: &str) -> ChapterNo {
        ChapterNo::parse(s).unwrap()
    }

    /// The shape OpenComic 1.6.5 wrote on semochy, plus keys that aren't this series' chapters.
    const FIXTURE: &str = r#"{
      "/dl/Test/Test - Ch 1.cbz": {"index": 24, "path": "/dl/Test/Test - Ch 1.cbz/024.jpg",
        "lastReading": 1794097176529, "ebook": false, "progress": 0, "chapterIndex": 0,
        "chapterProgress": 0, "page": 24, "pages": 24, "percent": 100, "completed": true},
      "/dl/Test/Test - Ch 2.cbz": {"index": 5, "path": "/dl/Test/Test - Ch 2.cbz/005.jpg",
        "lastReading": 1794097300000, "page": 5, "pages": 19, "completed": false},
      "/dl/Test": {"path": "Test - Ch 1.cbz/024.jpg", "page": 0, "pages": 0,
        "completed": false, "lastReading": 1794097176529},
      "/dl/Other/Other - Ch 1.cbz": {"lastReading": 1, "page": 1, "pages": 9, "completed": false},
      "/app/main/resources/app.asar.unpacked/Pepper & Carrot": {"pages": 11, "lastReading": 2},
      "/dl/Test/Test - Ch 3.cbz": {"lastReading": 3}
    }"#;

    fn test_chapter(path: &Path) -> Option<ChapterNo> {
        crate::download::chapter_of(Path::new("/dl"), "Test", path)
    }

    #[test]
    fn detects_opencomic() {
        for viewer in [
            "flatpak run app.opencomic.OpenComic",
            "open -a OpenComic",
            "opencomic",
        ] {
            assert!(is_opencomic(Some(viewer)), "{viewer}");
        }
        assert!(!is_opencomic(Some("YACReader")));
        assert!(!is_opencomic(None));
    }

    #[test]
    fn storage_candidates_cover_flatpak_and_native() {
        assert_eq!(
            storage_candidates(Path::new("/home/sem"), Path::new("/home/sem/.config")),
            [
                Path::new("/home/sem/.var/app/app.opencomic.OpenComic/config/OpenComic/storage"),
                Path::new("/home/sem/.config/OpenComic/storage"),
            ]
        );
    }

    #[test]
    fn parses_progress_fixture() {
        let mut found = entries(FIXTURE, test_chapter).unwrap();
        found.sort_by_key(|e| e.chapter);
        assert_eq!(
            found,
            [
                Entry {
                    chapter: ch("1"),
                    page: 24,
                    pages: 24,
                    completed: true,
                    last_reading_ms: 1794097176529,
                },
                Entry {
                    chapter: ch("2"),
                    page: 5,
                    pages: 19,
                    completed: false,
                    last_reading_ms: 1794097300000,
                },
            ],
            "the folder, other series, samples and incomplete entries are left out"
        );
        assert!(entries("{\"half", test_chapter).is_err());
        assert!(entries("", test_chapter).is_err());
    }

    fn entry(n: &str, completed: bool, at: i64) -> Entry {
        Entry {
            chapter: ch(n),
            page: 1,
            pages: 10,
            completed,
            last_reading_ms: at,
        }
    }

    #[test]
    fn current_chapter_moves_on_after_completed() {
        let chapters = [ch("1"), ch("2"), ch("3")];
        let current = |es: &[Entry]| current_chapter(es, &chapters);
        assert_eq!(current(&[]), None);
        assert_eq!(
            current(&[entry("1", true, 5), entry("2", false, 3)]),
            Some(ch("2"))
        );
        assert_eq!(
            current(&[entry("1", true, 1), entry("2", false, 3)]),
            Some(ch("2"))
        );
        assert_eq!(
            current(&[entry("2", false, 9), entry("1", true, 3)]),
            Some(ch("2"))
        );
        assert_eq!(
            current(&[entry("3", true, 9)]),
            Some(ch("3")),
            "nothing after the last"
        );
        assert_eq!(
            current_chapter(&[entry("10", true, 1)], &[ch("10"), ch("10.5"), ch("11")]),
            Some(ch("10.5"))
        );
    }

    #[test]
    fn sort_check() {
        assert!(!sorts_half_chapters_wrong(r#"{"sortReading": "numeric"}"#));
        assert!(sorts_half_chapters_wrong(
            r#"{"sortReading": "name-numeric"}"#
        ));
        assert!(
            sorts_half_chapters_wrong("{}"),
            "the default is name-numeric"
        );
        assert!(!sorts_half_chapters_wrong("not json"), "unknown: no tip");
    }
}
