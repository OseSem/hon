//! What AniList knows about a series, kept on disk so each is asked for once: cover and
//! banner art, a dominant color, and the facts a series page shows. Sources give none of
//! this. Also AniList's trending manga, for the Discover screen.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Color;
use iced::widget::image;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

const ANILIST: &str = hon_core::suggest::ANILIST;
/// AniList allows 90 requests a minute; a library page asks for a few dozen at most.
const AT_ONCE: usize = 3;
/// Bumped when what's cached changes shape, so older entries are fetched again.
const VERSION: u64 = 2;
const TRENDING_FOR: i64 = 12 * 3600;

const FIELDS: &str = "fragment m on Media { id title { english romaji } description(asHtml: false) \
                      genres averageScore status startDate { year } bannerImage \
                      coverImage { extraLarge color } }";
const BY_ID: &str = "query ($id: Int) { Media(id: $id, type: MANGA) { ...m } }";
const BY_TITLE: &str = "query ($q: String) { Media(search: $q, type: MANGA, isAdult: false, \
                        sort: SEARCH_MATCH) { ...m } }";
const TRENDING: &str = "query { Page(perPage: 24) { media(type: MANGA, sort: TRENDING_DESC, \
                        isAdult: false, format_in: [MANGA], countryOfOrigin: \"JP\") { ...m } } }";

/// The facts about a series.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Info {
    pub description: Option<String>,
    pub genres: Vec<String>,
    /// Out of 100.
    pub score: Option<u32>,
    pub status: Option<String>,
    pub year: Option<i32>,
    color: Option<String>,
    banner_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Cover {
    pub image: Option<image::Handle>,
    /// Wide key art, when AniList has it and it was fetched.
    pub banner: Option<image::Handle>,
    /// The cover shrunk and blurred, to stretch behind a header that has no banner.
    pub backdrop: Option<image::Handle>,
    pub color: Option<Color>,
    pub info: Info,
}

impl Cover {
    pub fn wants_banner(&self) -> bool {
        self.banner.is_none() && self.info.banner_url.is_some()
    }
}

/// A trending manga: its title and the key its cover is cached under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trend {
    pub title: String,
    pub key: String,
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

    fn base(&self, key: &str) -> PathBuf {
        self.dir.join(file_name(key))
    }

    /// What's known about the series under `key`: from disk, else from AniList by its
    /// linked entry or by `title`. Empty on network errors, so it's tried again later.
    pub async fn get(&self, key: &str, title: &str, anilist_id: Option<i64>) -> Cover {
        let base = self.base(key);
        if let Some(cover) = load(base.clone()).await {
            return cover;
        }
        let _turn = self.gate.acquire().await;
        let body = match anilist_id {
            Some(id) => json!({ "query": format!("{BY_ID} {FIELDS}"), "variables": { "id": id } }),
            None => json!({ "query": format!("{BY_TITLE} {FIELDS}"), "variables": { "q": title } }),
        };
        let media = match self.ask(body).await {
            Ok(value) => value["data"]["Media"].clone(),
            Err(_) => return Cover::default(),
        };
        if media.is_null() {
            let _ = save(&base, &Info::default(), None);
            return Cover::default();
        }
        self.store(&base, &media).await;
        load(base).await.unwrap_or_default()
    }

    /// The series' banner, fetching it when AniList has one that isn't on disk yet.
    pub async fn banner(&self, key: &str) -> Option<image::Handle> {
        let base = self.base(key);
        let path = base.with_extension("banner");
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return Some(image::Handle::from_bytes(bytes));
        }
        let meta = tokio::fs::read(base.with_extension("json")).await.ok()?;
        let info: Info = serde_json::from_slice(&meta).ok()?;
        let bytes = self.download(info.banner_url.as_deref()?).await.ok()?;
        let _ = tokio::fs::write(&path, &bytes).await;
        Some(image::Handle::from_bytes(bytes))
    }

    /// AniList's trending manga, at most `TRENDING_FOR` seconds old. Their covers are
    /// cached as a side effect, so `get` finds them on disk.
    pub async fn trending(&self) -> Vec<Trend> {
        let path = self.dir.join("trending.json");
        let now = hon_core::db::now();
        if let Ok(bytes) = tokio::fs::read(&path).await
            && let Ok(saved) = serde_json::from_slice::<(i64, Vec<Trend>)>(&bytes)
            && now - saved.0 < TRENDING_FOR
        {
            return saved.1;
        }
        let Ok(value) = self
            .ask(json!({ "query": format!("{TRENDING} {FIELDS}") }))
            .await
        else {
            return Vec::new();
        };
        let mut trends = Vec::new();
        for media in value["data"]["Page"]["media"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let title = &media["title"];
            let Some(title) = title["english"].as_str().or(title["romaji"].as_str()) else {
                continue;
            };
            let key = hon_core::model::normalize_title(title);
            let base = self.base(&key);
            if !base.with_extension("json").exists() {
                self.store(&base, media).await;
            }
            trends.push(Trend {
                title: title.to_string(),
                key,
            });
        }
        if !trends.is_empty()
            && let Ok(bytes) = serde_json::to_vec(&(now, &trends))
        {
            let _ = tokio::fs::create_dir_all(&self.dir).await;
            let _ = tokio::fs::write(&path, bytes).await;
        }
        trends
    }

    async fn ask(&self, body: Value) -> anyhow::Result<Value> {
        let response = self.http.post(ANILIST).json(&body).send().await?;
        let status = response.status();
        // AniList answers a search with no match as a 404 with `Media: null`.
        if !status.is_success() && status.as_u16() != 404 {
            anyhow::bail!("AniList answered {status}");
        }
        Ok(response.json().await?)
    }

    async fn download(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        Ok(response.bytes().await?.to_vec())
    }

    /// Writes a media's facts and cover to disk. Without the cover nothing is written, so
    /// it's fetched again later.
    async fn store(&self, base: &Path, media: &Value) {
        let cover = &media["coverImage"];
        let Some(url) = cover["extraLarge"].as_str() else {
            return;
        };
        let Ok(bytes) = self.download(url).await else {
            return;
        };
        let info = Info {
            description: media["description"]
                .as_str()
                .map(clean)
                .filter(|d| !d.is_empty()),
            genres: media["genres"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect(),
            score: media["averageScore"].as_u64().map(|s| s as u32),
            status: media["status"]
                .as_str()
                .and_then(status)
                .map(str::to_string),
            year: media["startDate"]["year"].as_i64().map(|y| y as i32),
            color: cover["color"].as_str().map(str::to_string),
            banner_url: media["bannerImage"].as_str().map(str::to_string),
        };
        let _ = save(base, &info, Some(&bytes));
    }
}

fn status(s: &str) -> Option<&'static str> {
    Some(match s {
        "FINISHED" => "Finished",
        "RELEASING" => "Ongoing",
        "NOT_YET_RELEASED" => "Upcoming",
        "CANCELLED" => "Cancelled",
        "HIATUS" => "On hiatus",
        _ => return None,
    })
}

/// AniList's description as plain text: tags and the trailing source credit dropped,
/// whitespace collapsed.
fn clean(description: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in description.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&#039;", "'");
    let text = match out.find("(Source:") {
        Some(i) => &out[..i],
        None => &out,
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A small, soft version of the cover: drawn large, it reads as the cover's colors only.
fn backdrop(bytes: &[u8]) -> Option<image::Handle> {
    let img = ::image::load_from_memory(bytes).ok()?;
    let small = img.resize_exact(36, 52, ::image::imageops::FilterType::Triangle);
    let soft = ::image::imageops::blur(&small.into_rgba8(), 2.4);
    let (w, h) = soft.dimensions();
    Some(image::Handle::from_rgba(w, h, soft.into_raw()))
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

async fn load(base: PathBuf) -> Option<Cover> {
    tokio::task::spawn_blocking(move || cached(&base))
        .await
        .ok()
        .flatten()
}

/// `<base>.json` says what's known; `<base>.img` and `<base>.banner` are the art, when
/// there is any. `None` when nothing current is cached.
fn cached(base: &Path) -> Option<Cover> {
    let meta: Value =
        serde_json::from_slice(&std::fs::read(base.with_extension("json")).ok()?).ok()?;
    if meta["v"].as_u64() != Some(VERSION) {
        return None;
    }
    let info: Info = serde_json::from_value(meta).ok()?;
    let bytes = std::fs::read(base.with_extension("img")).ok();
    Some(Cover {
        backdrop: bytes.as_deref().and_then(backdrop),
        // From bytes: iced picks a decoder by file extension, and `.img` names none.
        image: bytes.map(image::Handle::from_bytes),
        banner: std::fs::read(base.with_extension("banner"))
            .ok()
            .map(image::Handle::from_bytes),
        color: info.color.as_deref().and_then(parse_hex),
        info,
    })
}

fn save(base: &Path, info: &Info, cover: Option<&[u8]>) -> anyhow::Result<()> {
    if let Some(dir) = base.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(bytes) = cover {
        std::fs::write(base.with_extension("img"), bytes)?;
    }
    let mut meta = serde_json::to_value(info)?;
    meta["v"] = VERSION.into();
    std::fs::write(base.with_extension("json"), meta.to_string())?;
    Ok(())
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
    fn descriptions_lose_their_markup_and_source_credit() {
        let raw =
            "A boy meets a girl.<br><br>\n<i>They</i> talk &amp; laugh.\n\n(Source: Kodansha)";
        assert_eq!(clean(raw), "A boy meets a girl. They talk & laugh.");
        assert_eq!(clean(""), "");
    }

    #[test]
    fn what_is_known_round_trips_and_old_entries_are_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("x");
        assert!(cached(&base).is_none());

        save(&base, &Info::default(), None).unwrap();
        let miss = cached(&base).unwrap();
        assert!(miss.image.is_none() && miss.color.is_none(), "a known miss");

        let info = Info {
            description: Some("About it.".into()),
            genres: vec!["Romance".into()],
            score: Some(84),
            status: Some("Ongoing".into()),
            year: Some(2021),
            color: Some("#102030".into()),
            banner_url: Some("https://example.invalid/b.jpg".into()),
        };
        save(&base, &info, Some(b"img")).unwrap();
        let cover = cached(&base).unwrap();
        assert!(cover.image.is_some());
        assert!(cover.backdrop.is_none(), "not an image, so no backdrop");
        assert_eq!(cover.color, Some(Color::from_rgb8(0x10, 0x20, 0x30)));
        assert_eq!(cover.info, info);
        assert!(cover.wants_banner());

        std::fs::write(base.with_extension("json"), r##"{"color":"#102030"}"##).unwrap();
        assert!(
            cached(&base).is_none(),
            "written before the facts were kept"
        );
    }
}
