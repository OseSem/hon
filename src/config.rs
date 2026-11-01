use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Pdf,
    Cbz,
    Images,
}

pub struct Paths {
    pub config_file: PathBuf,
    pub db_file: PathBuf,
    pub default_download_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> anyhow::Result<Paths> {
        if let Some(root) = std::env::var_os("HON_HOME") {
            return Ok(Paths::under(Path::new(&root)));
        }
        let config = dirs::config_dir().ok_or_else(|| anyhow!("no config directory"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow!("no data directory"))?;
        let downloads = dirs::download_dir()
            .or_else(dirs::home_dir)
            .ok_or_else(|| anyhow!("no download or home directory"))?;
        Ok(Paths {
            config_file: config.join("hon").join("config.toml"),
            db_file: data.join("hon").join("hon.db"),
            default_download_dir: downloads.join("hon"),
        })
    }

    pub fn under(root: &Path) -> Paths {
        Paths {
            config_file: root.join("config.toml"),
            db_file: root.join("hon.db"),
            default_download_dir: root.join("downloads"),
        }
    }
}

pub struct Config {
    pub format: Format,
    pub viewer: Option<String>,
    pub download_dir: PathBuf,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    format: Format,
    viewer: Option<String>,
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
        Ok(Config {
            format: file.format,
            viewer: file.viewer,
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
        assert_eq!(config.download_dir, paths.default_download_dir);
    }

    #[test]
    fn full_file_parses_and_expands_tilde() {
        let (_dir, paths) = write_config(
            "format = \"cbz\"\nviewer = \"open -a YACReader\"\ndownload_dir = \"~/manga\"\n",
        );
        let config = Config::load(&paths).unwrap();
        assert_eq!(config.format, Format::Cbz);
        assert_eq!(config.viewer.as_deref(), Some("open -a YACReader"));
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
    fn unknown_format_is_error() {
        let (_dir, paths) = write_config("format = \"epub\"\n");
        assert!(Config::load(&paths).is_err());
    }
}
