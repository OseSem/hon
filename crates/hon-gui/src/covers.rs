//! Cover art and color for a series, from AniList, kept on disk so each is fetched once.
//! Sources don't give covers; AniList has one for nearly every manga, plus a dominant color.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Color;
use iced::widget::image;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

const ANILIST: &str = hon_core::suggest::ANILIST;
/// AniList allows 90 requests a minute; a library page asks for a few dozen at most.
const AT_ONCE: usize = 3;

const BY_ID: &str =
    "query ($id: Int) { Media(id: $id, type: MANGA) { coverImage { extraLarge color } } }";
const BY_TITLE: &str = "query ($q: String) { Media(search: $q, type: MANGA, isAdult: false, \
                        sort: SEARCH_MATCH) { coverImage { extraLarge color } } }";

#[derive(Debug, Clone, Default)]
pub struct Cover {
    pub image: Option<image::Handle>,
    pub color: Option<Color>,
}

#[derive(Clone)]
pub struct Covers {
    dir: PathBuf,
    http: reqwest::Client,
    gate: Arc<Semaphore>,
}

impl Covers {
    pub fn new(dir: PathBuf) -> Covers {
        Covers {
            dir,
            http: hon_core::sources::http_client(),
            gate: Arc::new(Semaphore::new(AT_ONCE)),
        }
    }

    /// The cover for the series known by `key`: from disk, else from AniList by its
    /// linked entry or by `title`. Nothing on network errors, so it's tried again later.
    pub async fn get(&self, key: &str, title: &str, anilist_id: Option<i64>) -> Cover {
        let base = self.dir.join(file_name(key));
        if let Some(cover) = cached(&base) {
            return cover;
        }
        let _turn = self.gate.acquire().await;
        match self.fetch(title, anilist_id).await {
            Ok(Some((bytes, color))) => {
                let _ = save(&base, Some(&bytes), color.as_deref());
                Cover {
                    image: Some(image::Handle::from_bytes(bytes)),
                    color: color.as_deref().and_then(parse_hex),
                }
            }
            Ok(None) => {
                let _ = save(&base, None, None);
                Cover::default()
            }
            Err(_) => Cover::default(),
        }
    }

    /// The cover's bytes and color; `None` when AniList has no such manga.
    async fn fetch(
        &self,
        title: &str,
        anilist_id: Option<i64>,
    ) -> anyhow::Result<Option<(Vec<u8>, Option<String>)>> {
        let body = match anilist_id {
            Some(id) => json!({ "query": BY_ID, "variables": { "id": id } }),
            None => json!({ "query": BY_TITLE, "variables": { "q": title } }),
        };
        let response = self.http.post(ANILIST).json(&body).send().await?;
        let status = response.status();
        let value: Value = response.json().await?;
        let cover = &value["data"]["Media"]["coverImage"];
        let Some(url) = cover["extraLarge"].as_str() else {
            // AniList answers a search with no match as a 404 with an error.
            if status.as_u16() == 404 {
                return Ok(None);
            }
            anyhow::bail!("AniList answered {status}");
        };
        let bytes = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec();
        Ok(Some((bytes, cover["color"].as_str().map(str::to_string))))
    }
}

fn file_name(key: &str) -> String {
    let name: String = key
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .take(80)
        .collect();
    if name.is_empty() {
        "untitled".into()
    } else {
        name
    }
}

/// `<base>.json` says what's known; `<base>.img` is the image when there is one.
fn cached(base: &Path) -> Option<Cover> {
    let meta: Value =
        serde_json::from_slice(&std::fs::read(base.with_extension("json")).ok()?).ok()?;
    let image = base.with_extension("img");
    Some(Cover {
        // From bytes: iced picks a decoder by file extension, and `.img` names none.
        image: std::fs::read(image).ok().map(image::Handle::from_bytes),
        color: meta["color"].as_str().and_then(parse_hex),
    })
}

fn save(base: &Path, bytes: Option<&[u8]>, color: Option<&str>) -> std::io::Result<()> {
    if let Some(dir) = base.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(bytes) = bytes {
        std::fs::write(base.with_extension("img"), bytes)?;
    }
    std::fs::write(
        base.with_extension("json"),
        json!({ "color": color }).to_string(),
    )
}

pub fn parse_hex(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::from_rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors_parse() {
        assert_eq!(parse_hex("#ff8000"), Some(Color::from_rgb8(255, 128, 0)));
        assert_eq!(parse_hex("ff8000"), None);
        assert_eq!(parse_hex("#fff"), None);
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("solo leveling"), "solo-leveling");
        assert_eq!(file_name("../x"), "---x");
        assert_eq!(file_name(""), "untitled");
    }

    #[test]
    fn a_known_miss_is_cached_without_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("x");
        assert!(cached(&base).is_none());
        save(&base, None, None).unwrap();
        let cover = cached(&base).unwrap();
        assert!(cover.image.is_none() && cover.color.is_none());
        save(&base, Some(b"img"), Some("#102030")).unwrap();
        let cover = cached(&base).unwrap();
        assert!(cover.image.is_some());
        assert_eq!(cover.color, Some(Color::from_rgb8(0x10, 0x20, 0x30)));
    }
}
