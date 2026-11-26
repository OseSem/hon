//! Reading progress kept in step with a tracker (AniList), so another app logged in to the
//! same account (Mihon) sees what was read in hon and the other way round.
//!
//! A tracker keeps one number per series: chapters read, counting whole chapters. hon only
//! ever raises it. Each hon series is linked to one tracker entry the user picked; updates
//! wait in a queue in the database until they're sent, so reading never waits on the
//! network and nothing is lost offline.

use std::collections::HashMap;

use crate::db::{Db, Progress};
use crate::model::ChapterNo;

pub mod anilist;
#[cfg(any(test, feature = "test-util"))]
pub mod fake;
pub mod login;
pub mod sync;

/// The tracker's name in the database.
pub const ANILIST: &str = "anilist";

/// A tracker entry offered when linking a series.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub id: i64,
    pub title: String,
    /// `MANGA`, `ONE_SHOT`…, as the tracker names it.
    pub format: Option<String>,
    pub year: Option<i32>,
    /// The series' chapter count, once it's finished.
    pub chapters: Option<u32>,
}

/// Where the account stands on an entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// `None` while the entry isn't on the account's list.
    pub progress: Option<u32>,
    pub status: Option<Status>,
    pub chapters: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Current,
    Completed,
    Repeating,
    /// Planning, paused or dropped: hon sets reading once a chapter is read.
    Other,
}

/// Failures that end a round of sending: the rest would fail the same way.
#[derive(Debug, PartialEq, Eq)]
pub enum TrackError {
    LoggedOut,
    RateLimited,
}

impl std::fmt::Display for TrackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TrackError::LoggedOut => "AniList login expired; run `hon login anilist`",
            TrackError::RateLimited => "AniList is rate limiting; progress goes out later",
        })
    }
}

impl std::error::Error for TrackError {}

#[async_trait::async_trait]
pub trait Tracker: Send + Sync {
    /// The tracker's name in the database.
    fn name(&self) -> &'static str;
    async fn search(&self, title: &str) -> anyhow::Result<Vec<Match>>;
    async fn entry(&self, id: i64) -> anyhow::Result<Entry>;
    async fn save(&self, id: i64, progress: u32, status: Status) -> anyhow::Result<()>;
}

/// Whether `e` ends a round of sending.
pub fn stops_sending(e: &anyhow::Error) -> bool {
    e.downcast_ref::<TrackError>().is_some()
}

/// The whole chapters a chapter number counts as: 10.5 is 10.
pub fn whole(n: ChapterNo) -> u32 {
    n.value().floor().clamp(0.0, u32::MAX as f64) as u32
}

/// hon's number for a tracker: the highest chapter read, in whole chapters; 0 when none is.
pub fn local_number(progress: &HashMap<ChapterNo, Progress>) -> u32 {
    progress
        .iter()
        .filter(|(_, p)| p.read)
        .map(|(&n, _)| whole(n))
        .max()
        .unwrap_or(0)
}

/// Takes in the tracker's number when it's ahead of hon's: every chapter of `chapters` up to
/// it is marked read (10.5 isn't covered by 10). Saved pages elsewhere stay. Returns whether
/// anything changed.
pub fn apply_remote(
    db: &Db,
    series: i64,
    chapters: &[ChapterNo],
    remote: u32,
) -> anyhow::Result<bool> {
    let progress = db.progress(series)?;
    if remote <= local_number(&progress) {
        return Ok(false);
    }
    let mut changed = false;
    for &n in chapters {
        let read = progress.get(&n).is_some_and(|p| p.read);
        if n.value() <= remote as f64 && !read {
            db.set_read(series, n, true)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Queues hon's number for the series when it's linked under `account` and ahead of the
/// tracker's number as last seen. Returns whether something was queued.
pub fn queue_local(db: &Db, tracker: &str, account: &str, series: i64) -> anyhow::Result<bool> {
    let Some(link) = db.link(series, tracker, account)? else {
        return Ok(false);
    };
    let number = local_number(&db.progress(series)?);
    if link.remote_id.is_none() || number <= link.synced.unwrap_or(0) {
        return Ok(false);
    }
    db.queue_progress(series, tracker, account, number)?;
    Ok(true)
}

/// The status to save with `progress`: completed and rereading stay; otherwise completed
/// once the last chapter is read, else reading.
pub fn push_status(entry: &Entry, progress: u32) -> Status {
    match entry.status {
        Some(s @ (Status::Completed | Status::Repeating)) => s,
        _ if entry.chapters.is_some_and(|c| c > 0 && progress >= c) => Status::Completed,
        _ => Status::Current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_ids::alpha;

    fn ch(s: &str) -> ChapterNo {
        ChapterNo::parse(s).unwrap()
    }

    fn series(db: &Db) -> i64 {
        db.series(&["k".to_string()], "K").unwrap().id
    }

    #[test]
    fn local_number_counts_whole_read_chapters() {
        let db = Db::open_in_memory().unwrap();
        let s = series(&db);
        assert_eq!(local_number(&db.progress(s).unwrap()), 0);
        db.set_read(s, ch("10.5"), true).unwrap();
        db.set_read(s, ch("3"), true).unwrap();
        db.save_page(s, ch("12"), &alpha(), 4, 20).unwrap();
        assert_eq!(local_number(&db.progress(s).unwrap()), 10);
    }

    #[test]
    fn apply_remote_marks_known_chapters_up_to_the_number() {
        let db = Db::open_in_memory().unwrap();
        let s = series(&db);
        let chapters: Vec<ChapterNo> = ["1", "2", "2.5", "3", "4"].map(ch).to_vec();
        db.save_page(s, ch("4"), &alpha(), 6, 20).unwrap();
        assert!(apply_remote(&db, s, &chapters, 2).unwrap());
        let p = db.progress(s).unwrap();
        assert!(p[&ch("1")].read && p[&ch("2")].read);
        assert!(!p.contains_key(&ch("2.5")), "2 doesn't cover 2.5");
        assert_eq!(p[&ch("4")].last_page, Some(6), "the saved page stays");
        assert!(!apply_remote(&db, s, &chapters, 2).unwrap(), "nothing new");
        assert!(!apply_remote(&db, s, &chapters, 1).unwrap(), "behind hon");
        assert!(!apply_remote(&db, s, &chapters, 0).unwrap());
    }

    #[test]
    fn apply_remote_never_unmarks_or_invents_chapters() {
        let db = Db::open_in_memory().unwrap();
        let s = series(&db);
        let chapters: Vec<ChapterNo> = ["50", "51"].map(ch).to_vec();
        assert!(!apply_remote(&db, s, &chapters, 10).unwrap());
        assert!(db.progress(s).unwrap().is_empty());
    }

    #[test]
    fn queue_local_only_for_linked_series_with_something_read() {
        let db = Db::open_in_memory().unwrap();
        let s = series(&db);
        db.set_read(s, ch("0"), true).unwrap();
        assert!(!queue_local(&db, ANILIST, "1", s).unwrap(), "not linked");
        db.set_link(s, ANILIST, "1", Some(9), None).unwrap();
        assert!(
            !queue_local(&db, ANILIST, "1", s).unwrap(),
            "0 isn't progress"
        );
        db.set_read(s, ch("7.5"), true).unwrap();
        assert!(queue_local(&db, ANILIST, "1", s).unwrap());
        assert_eq!(db.queued(ANILIST, "1").unwrap()[0].progress, 7);
        db.dequeue(s, ANILIST, "1", 7).unwrap();
        db.set_synced(s, ANILIST, "1", 9, 7).unwrap();
        assert!(
            !queue_local(&db, ANILIST, "1", s).unwrap(),
            "AniList has it already"
        );
        db.set_link(s, ANILIST, "1", None, None).unwrap();
        assert!(!queue_local(&db, ANILIST, "1", s).unwrap(), "skipped");
        assert!(
            !queue_local(&db, ANILIST, "2", s).unwrap(),
            "another account"
        );
    }

    #[test]
    fn push_status_keeps_finished_states() {
        let entry = |status, chapters| Entry {
            progress: Some(1),
            status,
            chapters,
        };
        assert_eq!(push_status(&entry(None, None), 5), Status::Current);
        assert_eq!(
            push_status(&entry(Some(Status::Other), Some(10)), 5),
            Status::Current
        );
        assert_eq!(
            push_status(&entry(Some(Status::Other), Some(10)), 10),
            Status::Completed
        );
        assert_eq!(
            push_status(&entry(Some(Status::Completed), None), 5),
            Status::Completed
        );
        assert_eq!(
            push_status(&entry(Some(Status::Repeating), Some(10)), 10),
            Status::Repeating
        );
        assert_eq!(push_status(&entry(None, Some(0)), 3), Status::Current);
    }
}
