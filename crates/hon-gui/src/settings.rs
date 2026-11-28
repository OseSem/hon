//! Reader preferences, kept in `gui.json` next to hon's database.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Pages stacked top to bottom, scrolled through.
    #[default]
    Strip,
    /// One page at a time, turned with keys or clicks.
    Pages,
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
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            mode: Mode::Strip,
            right_to_left: true,
            zoom: 1.0,
            two_pages: false,
            thumbnails: false,
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

    /// The saved settings; the defaults when there are none or they don't parse.
    pub fn load(&self) -> Settings {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, settings: &Settings) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_vec_pretty(settings)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert_eq!(store.load(), Settings::default());
        let changed = Settings {
            mode: Mode::Pages,
            right_to_left: false,
            zoom: 1.3,
            two_pages: true,
            thumbnails: true,
        };
        store.save(&changed).unwrap();
        assert_eq!(store.load(), changed);
        std::fs::write(dir.path().join("gui.json"), "{\"mode\":\"pages\"}").unwrap();
        assert_eq!(
            store.load(),
            Settings {
                mode: Mode::Pages,
                ..Settings::default()
            }
        );
    }
}
