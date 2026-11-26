//! The saved AniList login: a file only its owner can read.

use std::io::{self, Write};
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub struct Login {
    pub token: String,
    pub user_id: i64,
    pub name: String,
}

impl Login {
    /// What the database files links and queued progress under.
    pub fn account(&self) -> String {
        self.user_id.to_string()
    }
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("user_id", &self.user_id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The saved login; `None` when there's none.
pub fn load(path: &Path) -> anyhow::Result<Option<Login>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let login = serde_json::from_str(&text)
        .with_context(|| format!("{} is damaged; run `hon login anilist`", path.display()))?;
    Ok(Some(login))
}

pub fn save(path: &Path, login: &Login) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Written beside and renamed over, so the token is never in a file others can read.
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(serde_json::to_string(login)?.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Forgets the login; false when there was none.
pub fn remove(path: &Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("anilist.json");
        assert_eq!(load(&path).unwrap(), None);
        let login = Login {
            token: "secret".into(),
            user_id: 8,
            name: "sem".into(),
        };
        save(&path, &login).unwrap();
        assert_eq!(load(&path).unwrap(), Some(login.clone()));
        assert_eq!(login.account(), "8");
        assert!(
            !format!("{login:?}").contains("secret"),
            "Debug hides the token"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(remove(&path).unwrap());
        assert!(!remove(&path).unwrap());
        assert_eq!(load(&path).unwrap(), None);
    }

    #[test]
    fn damaged_file_says_how_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anilist.json");
        std::fs::write(&path, "{").unwrap();
        let e = load(&path).unwrap_err().to_string();
        assert!(e.ends_with("is damaged; run `hon login anilist`"), "{e}");
    }
}
