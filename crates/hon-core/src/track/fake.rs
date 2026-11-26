//! A tracker in memory, for tests.

use std::collections::HashMap;
use std::sync::Mutex;

use super::{ANILIST, Entry, Match, Status, TrackError, Tracker};

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
