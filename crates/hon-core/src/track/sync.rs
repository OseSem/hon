//! Sending queued progress to the tracker.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::{Tracker, push_status, stops_sending};
use crate::db::Db;

/// Sends what's queued for `account`, oldest first: each series to the entry it's linked to
/// now, never lowering the tracker's number. A series no longer linked is dropped from the
/// queue. Stops at the first failure, keeping it and the rest queued. Returns how many
/// entries were updated.
pub async fn flush(db: &Mutex<Db>, tracker: &dyn Tracker, account: &str) -> anyhow::Result<usize> {
    let name = tracker.name();
    let queued = lock(db).queued(name, account)?;
    let mut saved = 0;
    for q in queued {
        let Some(id) = q.remote_id else {
            lock(db).dequeue(q.series_id, name, account, q.progress)?;
            continue;
        };
        let entry = tracker.entry(id).await?;
        if !entry.progress.is_some_and(|p| p >= q.progress) {
            tracker
                .save(id, q.progress, push_status(&entry, q.progress))
                .await?;
            saved += 1;
        }
        lock(db).dequeue(q.series_id, name, account, q.progress)?;
    }
    Ok(saved)
}

fn lock(db: &Mutex<Db>) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum Msg {
    Flush,
    Wait(oneshot::Sender<()>),
}

/// A task that sends queued progress in the background, with its own database connection.
#[derive(Clone)]
pub struct Syncer {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Syncer {
    /// Starts the task. The receiver gets a line for each failure worth telling the user,
    /// not repeating one until a round succeeds.
    pub fn spawn(
        db_path: PathBuf,
        tracker: Arc<dyn Tracker>,
        account: String,
    ) -> (Syncer, mpsc::UnboundedReceiver<String>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (notes, notes_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let db = match Db::open(&db_path) {
                Ok(db) => Mutex::new(db),
                Err(e) => {
                    let _ = notes.send(format!("couldn't send progress to AniList: {e:#}"));
                    return;
                }
            };
            let mut last_note = None;
            while let Some(msg) = rx.recv().await {
                match msg {
                    Msg::Flush => match flush(&db, tracker.as_ref(), &account).await {
                        Ok(_) => last_note = None,
                        Err(e) => {
                            let note = if stops_sending(&e) {
                                e.to_string()
                            } else {
                                format!("couldn't send progress to AniList: {e:#}")
                            };
                            if last_note.as_ref() != Some(&note) {
                                let _ = notes.send(note.clone());
                                last_note = Some(note);
                            }
                        }
                    },
                    Msg::Wait(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        (Syncer { tx }, notes_rx)
    }

    /// Sends what's queued, in the background.
    pub fn flush(&self) {
        let _ = self.tx.send(Msg::Flush);
    }

    /// Sends what's queued and waits for it, at most `timeout`. Returns whether it finished.
    pub async fn finish(&self, timeout: Duration) -> bool {
        let (done, wait) = oneshot::channel();
        self.flush();
        if self.tx.send(Msg::Wait(done)).is_err() {
            return true;
        }
        tokio::time::timeout(timeout, wait).await.is_ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::model::ChapterNo;
    use crate::track::{ANILIST, Entry, Match, Status, TrackError};

    /// A tracker in memory: entries by id, every save recorded.
    #[derive(Default)]
    pub struct Fake {
        pub entries: Mutex<HashMap<i64, Entry>>,
        pub saves: Mutex<Vec<(i64, u32, Status)>>,
        pub fail: Mutex<Option<TrackError>>,
        pub matches: Vec<Match>,
    }

    #[async_trait::async_trait]
    impl Tracker for Fake {
        fn name(&self) -> &'static str {
            ANILIST
        }
        async fn search(&self, _title: &str) -> anyhow::Result<Vec<Match>> {
            Ok(self.matches.clone())
        }
        async fn entry(&self, id: i64) -> anyhow::Result<Entry> {
            if let Some(e) = self.fail.lock().unwrap().take() {
                return Err(e.into());
            }
            Ok(self
                .entries
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .unwrap_or(Entry {
                    progress: None,
                    status: None,
                    chapters: None,
                }))
        }
        async fn save(&self, id: i64, progress: u32, status: Status) -> anyhow::Result<()> {
            self.saves.lock().unwrap().push((id, progress, status));
            self.entries.lock().unwrap().insert(
                id,
                Entry {
                    progress: Some(progress),
                    status: Some(status),
                    chapters: None,
                },
            );
            Ok(())
        }
    }

    fn entry(progress: u32) -> Entry {
        Entry {
            progress: Some(progress),
            status: Some(Status::Current),
            chapters: None,
        }
    }

    fn setup() -> (Mutex<Db>, i64, i64) {
        let db = Db::open_in_memory().unwrap();
        let a = db.series(&["a".into()], "A").unwrap().id;
        let b = db.series(&["b".into()], "B").unwrap().id;
        (Mutex::new(db), a, b)
    }

    #[tokio::test]
    async fn sends_to_the_current_link_and_never_lowers() {
        let (db, a, b) = setup();
        let fake = Fake::default();
        fake.entries.lock().unwrap().insert(20, entry(50));
        {
            let db = db.lock().unwrap();
            db.set_link(a, ANILIST, "1", Some(10), None).unwrap();
            db.queue_progress(a, ANILIST, "1", 5).unwrap();
            db.set_link(a, ANILIST, "1", Some(11), None).unwrap(); // relinked
            db.set_link(b, ANILIST, "1", Some(20), None).unwrap();
            db.queue_progress(b, ANILIST, "1", 40).unwrap();
        }
        assert_eq!(flush(&db, &fake, "1").await.unwrap(), 1);
        assert_eq!(*fake.saves.lock().unwrap(), [(11, 5, Status::Current)]);
        assert_eq!(fake.entries.lock().unwrap()[&20].progress, Some(50));
        assert!(db.lock().unwrap().queued(ANILIST, "1").unwrap().is_empty());
    }

    #[tokio::test]
    async fn unlinked_rows_are_dropped_unsent() {
        let (db, a, _) = setup();
        let fake = Fake::default();
        {
            let db = db.lock().unwrap();
            db.set_link(a, ANILIST, "1", Some(10), None).unwrap();
            db.queue_progress(a, ANILIST, "1", 5).unwrap();
            db.set_link(a, ANILIST, "1", None, None).unwrap();
        }
        assert_eq!(flush(&db, &fake, "1").await.unwrap(), 0);
        assert!(fake.saves.lock().unwrap().is_empty());
        assert!(db.lock().unwrap().queued(ANILIST, "1").unwrap().is_empty());
    }

    #[tokio::test]
    async fn logged_out_keeps_the_queue() {
        let (db, a, _) = setup();
        let fake = Fake::default();
        *fake.fail.lock().unwrap() = Some(TrackError::LoggedOut);
        {
            let db = db.lock().unwrap();
            db.set_link(a, ANILIST, "1", Some(10), None).unwrap();
            db.queue_progress(a, ANILIST, "1", 5).unwrap();
        }
        let e = flush(&db, &fake, "1").await.unwrap_err();
        assert!(stops_sending(&e));
        assert_eq!(db.lock().unwrap().queued(ANILIST, "1").unwrap().len(), 1);
        assert_eq!(flush(&db, &fake, "1").await.unwrap(), 1, "sent next time");
    }

    #[tokio::test]
    async fn syncer_sends_in_the_background_and_reports_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hon.db");
        let db = Db::open(&path).unwrap();
        let a = db.series(&["a".into()], "A").unwrap().id;
        db.set_link(a, ANILIST, "1", Some(10), None).unwrap();
        db.set_read(a, ChapterNo::parse("3").unwrap(), true)
            .unwrap();
        crate::track::queue_local(&db, ANILIST, "1", a).unwrap();

        let fake = Arc::new(Fake::default());
        *fake.fail.lock().unwrap() = Some(TrackError::RateLimited);
        let (syncer, mut notes) = Syncer::spawn(path, fake.clone(), "1".into());
        assert!(syncer.finish(Duration::from_secs(5)).await);
        assert_eq!(
            notes.try_recv().unwrap(),
            "AniList is rate limiting; progress goes out later"
        );
        assert!(syncer.finish(Duration::from_secs(5)).await);
        assert_eq!(*fake.saves.lock().unwrap(), [(10, 3, Status::Current)]);
        assert!(notes.try_recv().is_err());
        assert!(db.queued(ANILIST, "1").unwrap().is_empty());
    }
}
