//! Preferences, kept in `gui.json` next to hon's database: how the reader shows pages, how
//! the library is sorted and shown, and which series are favorites.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Pages stacked top to bottom, scrolled through.
    #[default]
    Strip,
    /// One page (or two) at a time, turned with keys or clicks.
    Pages,
}

/// What's behind the pages in the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backdrop {
    #[default]
    Ink,
    Black,
    Paper,
}

/// How a page fits the window in page mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// The whole page on screen.
    #[default]
    Page,
    /// As wide as the window, scrolled when taller.
    Width,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    /// Turn pages right to left, as printed manga reads.
    pub right_to_left: bool,
    /// Strip width as a multiple of the default.
    pub zoom: f32,
    /// Show two pages side by side in page mode.
    pub two_pages: bool,
    /// Show the page thumbnails beside the reader.
    pub thumbnails: bool,
    pub backdrop: Backdrop,
    pub fit: Fit,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            mode: Mode::Strip,
            right_to_left: true,
            zoom: 1.0,
            two_pages: false,
            thumbnails: false,
            backdrop: Backdrop::Ink,
            fit: Fit::Page,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Recent,
    Title,
    Progress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    #[default]
    Covers,
    List,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub reader: Settings,
    pub sort: Sort,
    pub view: View,
    /// Series ids.
    pub favorites: BTreeSet<i64>,
    /// Look covers up on AniList.
    pub covers: bool,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs {
            reader: Settings::default(),
            sort: Sort::Recent,
            view: View::Covers,
            favorites: BTreeSet::new(),
            covers: true,
        }
    }
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Store {
        Store {
            path: dir.join("gui.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved preferences; the defaults when there are none or they don't parse.
    pub fn load(&self) -> Prefs {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, prefs: &Prefs) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_vec_pretty(prefs)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip_and_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert_eq!(store.load(), Prefs::default());
        let mut changed = Prefs::default();
        changed.reader.mode = Mode::Pages;
        changed.reader.backdrop = Backdrop::Paper;
        changed.sort = Sort::Title;
        changed.favorites.insert(7);
        store.save(&changed).unwrap();
        assert_eq!(store.load(), changed);
        std::fs::write(
            dir.path().join("gui.json"),
            r#"{"reader":{"mode":"pages"},"view":"list"}"#,
        )
        .unwrap();
        let partial = store.load();
        assert_eq!(partial.reader.mode, Mode::Pages);
        assert!(partial.reader.right_to_left);
        assert_eq!(partial.view, View::List);
        assert!(partial.covers);
    }
}
