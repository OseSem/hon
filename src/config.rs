use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

use crate::viewer::opencomic;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Pdf,
    Cbz,
    Images,
}

/// What Enter on a chapter (and `hon read`) opens it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Reader {
    /// hon's own reader.
    #[default]
    Terminal,
    /// The configured `viewer`, or the OS default app.
    Viewer,
}

pub struct Paths {
    pub config_file: PathBuf,
    pub db_file: PathBuf,
    pub default_download_dir: PathBuf,
    /// Where OpenComic may keep its storage folder, most likely first.
    pub opencomic_storage: Vec<PathBuf>,
}

impl Paths {
    pub fn detect() -> anyhow::Result<Paths> {
        Paths::detect_with(std::env::var_os("HON_HOME"))
    }

    /// `detect`, with `hon_home` as the value of `HON_HOME`.
    fn detect_with(hon_home: Option<OsString>) -> anyhow::Result<Paths> {
        if let Some(root) = hon_home {
            let paths = Paths::under(Path::new(&root));
            // HON_HOME moves hon's own files; OpenComic's stay where OpenComic keeps them.
            let Some((home, config)) = dirs::home_dir().zip(dirs::config_dir()) else {
                return Ok(paths);
            };
            return Ok(Paths {
                opencomic_storage: opencomic::storage_candidates(&home, &config),
                ..paths
            });
        }
        let config = dirs::config_dir().ok_or_else(|| anyhow!("no config directory"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow!("no data directory"))?;
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
        let downloads = dirs::download_dir().unwrap_or_else(|| home.clone());
        Ok(Paths {
            config_file: config.join("hon").join("config.toml"),
            db_file: data.join("hon").join("hon.db"),
            default_download_dir: downloads.join("hon"),
            opencomic_storage: opencomic::storage_candidates(&home, &config),
        })
    }

    pub fn under(root: &Path) -> Paths {
        Paths {
            config_file: root.join("config.toml"),
            db_file: root.join("hon.db"),
            default_download_dir: root.join("downloads"),
            opencomic_storage: vec![root.join("opencomic")],
        }
    }
}

pub struct Config {
    pub format: Format,
    pub viewer: Option<String>,
    pub reader: Reader,
    pub download_dir: PathBuf,
    /// OpenComic's storage folder, when `viewer` is OpenComic and the folder exists.
    pub opencomic: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    format: Format,
    viewer: Option<String>,
    #[serde(default)]
    reader: Reader,
    download_dir: Option<String>,
}

impl Config {
    pub fn load(paths: &Paths) -> anyhow::Result<Config> {
        let path = &paths.config_file;
        let file: ConfigFile = match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let download_dir = match file.download_dir {
            Some(dir) => expand_home(&dir)?,
            None => paths.default_download_dir.clone(),
        };
        let opencomic = opencomic::is_opencomic(file.viewer.as_deref())
            .then(|| {
                paths
                    .opencomic_storage
                    .iter()
                    .find(|dir| dir.is_dir())
                    .cloned()
            })
            .flatten();
        Ok(Config {
            format: file.format,
            opencomic,
            viewer: file.viewer,
            reader: file.reader,
            download_dir,
        })
    }
}

fn expand_home(path: &str) -> anyhow::Result<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => {
            let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory to expand ~"))?;
            Ok(home.join(rest))
        }
        None => Ok(PathBuf::from(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(contents: &str) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::write(&paths.config_file, contents).unwrap();
        (dir, paths)
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let config = Config::load(&paths).unwrap();
        assert_eq!(config.format, Format::Pdf);
        assert_eq!(config.viewer, None);
        assert_eq!(config.reader, Reader::Terminal);
        assert_eq!(config.download_dir, paths.default_download_dir);
    }

    #[test]
    fn full_file_parses_and_expands_tilde() {
        let (_dir, paths) = write_config(
            "format = \"cbz\"\nviewer = \"open -a YACReader\"\nreader = \"viewer\"\n\
             download_dir = \"~/manga\"\n",
        );
        let config = Config::load(&paths).unwrap();
        assert_eq!(config.format, Format::Cbz);
        assert_eq!(config.viewer.as_deref(), Some("open -a YACReader"));
        assert_eq!(config.reader, Reader::Viewer);
        assert_eq!(config.download_dir, dirs::home_dir().unwrap().join("manga"));
    }

    #[test]
    fn unknown_key_names_key_and_file() {
        let (_dir, paths) = write_config("colour = \"red\"\n");
        let err = format!("{:#}", Config::load(&paths).err().unwrap());
        assert!(err.contains("colour"), "{err}");
        assert!(
            err.contains(&paths.config_file.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn reader_terminal_parses_and_unknown_reader_is_error() {
        let (_dir, paths) = write_config("reader = \"terminal\"\n");
        assert_eq!(Config::load(&paths).unwrap().reader, Reader::Terminal);
        let (_dir, paths) = write_config("reader = \"preview\"\n");
        let err = format!("{:#}", Config::load(&paths).err().unwrap());
        assert!(err.contains("preview"), "{err}");
    }

    #[test]
    fn hon_home_still_finds_opencomic_where_opencomic_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::detect_with(Some(dir.path().as_os_str().to_owned())).unwrap();
        assert_eq!(paths.config_file, dir.path().join("config.toml"));
        let (home, config) = (dirs::home_dir().unwrap(), dirs::config_dir().unwrap());
        assert_eq!(
            paths.opencomic_storage,
            opencomic::storage_candidates(&home, &config),
            "HON_HOME moves hon's files, not OpenComic's"
        );
    }

    #[test]
    fn opencomic_storage_found_only_for_opencomic_viewer() {
        let (dir, paths) = write_config("viewer = \"flatpak run app.opencomic.OpenComic\"\n");
        assert_eq!(
            Config::load(&paths).unwrap().opencomic,
            None,
            "no folder yet"
        );
        std::fs::create_dir(dir.path().join("opencomic")).unwrap();
        assert_eq!(
            Config::load(&paths).unwrap().opencomic,
            Some(dir.path().join("opencomic"))
        );
        std::fs::write(&paths.config_file, "viewer = \"YACReader\"\n").unwrap();
        assert_eq!(Config::load(&paths).unwrap().opencomic, None);
    }

    #[test]
    fn unknown_format_is_error() {
        let (_dir, paths) = write_config("format = \"epub\"\n");
        assert!(Config::load(&paths).is_err());
    }
}
