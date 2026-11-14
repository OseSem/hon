//! Title suggestions while typing a search.
//!
//! MangaDex matches partial words ("frier" finds Frieren) and links most series to
//! AniList; AniList has the English titles, synonyms and popularity to rank them by.
//! One request to each, then titles that start like the typed text come first.

use std::collections::HashMap;

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::model::normalize_title;

pub const ANILIST: &str = "https://graphql.anilist.co";
pub const MANGADEX: &str = "https://api.mangadex.org";
const SHOWN: usize = 8;
/// Candidates asked of each service, before ranking.
const ASKED: usize = 20;
/// MangaDex tags of fan works: Doujinshi, Fan Colored.
const FAN_WORKS: [&str; 2] = [
    "b13b2a48-c720-44a9-9c77-39c9979373fb",
    "7b2ce280-79ef-4c09-9b58-12b7c23a9b78",
];
const QUERY: &str = "query ($q: String, $ids: [Int], $n: Int) {
  found: Page(perPage: $n) {
    media(search: $q, type: MANGA, sort: POPULARITY_DESC, isAdult: false,
          format_not_in: [NOVEL]) { ...m }
  }
  linked: Page(perPage: $n) {
    media(id_in: $ids, type: MANGA, format_not_in: [NOVEL]) { ...m }
  }
}
fragment m on Media {
  id
  title { english romaji }
  synonyms
  popularity
  countryOfOrigin
  format
  startDate { year }
}";

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub title: String,
    /// manga, manhwa, manhua or one-shot.
    pub kind: &'static str,
    pub year: Option<i32>,
}

pub struct Suggester {
    http: reqwest::Client,
    anilist: String,
    mangadex: String,
}

/// A series either service offered, with every title it goes by.
struct Candidate {
    shown: Suggestion,
    /// Normalized titles, the shown one included.
    names: Vec<String>,
    /// AniList's; `None` for series only MangaDex knows.
    popularity: Option<u32>,
}

impl Suggester {
    pub fn new(http: reqwest::Client, anilist: &str, mangadex: &str) -> Suggester {
        Suggester {
            http,
            anilist: anilist.to_string(),
            mangadex: mangadex.trim_end_matches('/').to_string(),
        }
    }

    /// Fails only when both services do; AniList's error is the one returned.
    pub async fn suggest(&self, prefix: &str) -> anyhow::Result<Vec<Suggestion>> {
        let from_mangadex = self.mangadex(prefix).await.unwrap_or_default();
        let ids: Vec<u64> = from_mangadex.iter().filter_map(|(_, id)| *id).collect();
        let mut candidates = match self.anilist(prefix, &ids).await {
            Ok(found) => found,
            Err(e) if from_mangadex.is_empty() => return Err(e),
            Err(_) => Vec::new(),
        };
        // Series AniList doesn't have (or wasn't asked about) stay, as MangaDex has them.
        candidates.extend(from_mangadex.into_iter().map(|(c, _)| c));
        Ok(rank(prefix, candidates))
    }

    /// Matches ordered by MangaDex follows, each with its AniList id when linked.
    async fn mangadex(&self, prefix: &str) -> anyhow::Result<Vec<(Candidate, Option<u64>)>> {
        let body: MdList = self
            .http
            .get(format!("{}/manga", self.mangadex))
            .query(&[
                ("title", prefix),
                ("limit", &ASKED.to_string()),
                ("order[followedCount]", "desc"),
            ])
            .query(&FAN_WORKS.map(|tag| ("excludedTags[]", tag)))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(body
            .data
            .into_iter()
            .map(|m| m.attributes.candidate())
            .collect())
    }

    async fn anilist(&self, prefix: &str, ids: &[u64]) -> anyhow::Result<Vec<Candidate>> {
        let variables = json!({ "q": prefix, "ids": ids, "n": ASKED });
        let res = self
            .http
            .post(&self.anilist)
            .header("Accept", "application/json")
            .json(&json!({ "query": QUERY, "variables": variables }))
            .send()
            .await
            .context("AniList")?;
        if res.status() == StatusCode::TOO_MANY_REQUESTS {
            bail!("AniList is rate limiting; slow down a moment");
        }
        let body: AlResponse = res
            .error_for_status()
            .context("AniList")?
            .json()
            .await
            .context("AniList's answer")?;
        let mut by_id = HashMap::new();
        for m in body
            .data
            .found
            .media
            .into_iter()
            .chain(body.data.linked.media)
        {
            by_id.entry(m.id).or_insert(m);
        }
        Ok(by_id.into_values().filter_map(AlMedia::candidate).collect())
    }
}

/// How well `names` match the typed text: 0 a title starts with it, 1 a word in a
/// title does, 2 neither (the service matched something else).
fn tier(typed: &str, names: &[String]) -> u8 {
    if names.iter().any(|n| n.starts_with(typed)) {
        0
    } else if names
        .iter()
        .any(|n| format!(" {n}").contains(&format!(" {typed}")))
    {
        1
    } else {
        2
    }
}

/// Best match first, most popular first within a match; one entry per series.
/// Loose matches only fill in when nothing matches what was typed.
fn rank(prefix: &str, candidates: Vec<Candidate>) -> Vec<Suggestion> {
    let typed = normalize_title(prefix);
    let mut tiered: Vec<(u8, Candidate)> = candidates
        .into_iter()
        .map(|c| (tier(&typed, &c.names), c))
        .collect();
    tiered.sort_by_key(|(t, c)| (*t, std::cmp::Reverse(c.popularity)));
    let best = tiered.first().map_or(2, |(t, _)| *t);
    let mut kept: Vec<Candidate> = Vec::new();
    for (t, c) in tiered {
        if t == 2 && best < 2 {
            break;
        }
        if kept
            .iter()
            .any(|k| k.names.iter().any(|n| c.names.contains(n)))
        {
            continue;
        }
        kept.push(c);
        if kept.len() == SHOWN {
            break;
        }
    }
    kept.into_iter().map(|c| c.shown).collect()
}

fn names<'a>(titles: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in titles.into_iter().map(|t| normalize_title(t)) {
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

#[derive(Deserialize)]
struct AlResponse {
    data: AlData,
}

#[derive(Deserialize)]
struct AlData {
    found: AlPage,
    linked: AlPage,
}

#[derive(Deserialize)]
struct AlPage {
    media: Vec<AlMedia>,
}

#[derive(Deserialize)]
struct AlMedia {
    id: u64,
    title: AlTitle,
    #[serde(default)]
    synonyms: Vec<String>,
    popularity: Option<u32>,
    #[serde(rename = "countryOfOrigin")]
    country: Option<String>,
    format: Option<String>,
    #[serde(rename = "startDate")]
    start: Option<AlDate>,
}

#[derive(Deserialize)]
struct AlTitle {
    english: Option<String>,
    romaji: Option<String>,
}

#[derive(Deserialize)]
struct AlDate {
    year: Option<i32>,
}

impl AlMedia {
    fn candidate(self) -> Option<Candidate> {
        let title = self.title.english.clone().or(self.title.romaji.clone())?;
        let kind = match (self.format.as_deref(), self.country.as_deref()) {
            (Some("ONE_SHOT"), _) => "one-shot",
            (_, Some("KR")) => "manhwa",
            (_, Some("CN" | "TW")) => "manhua",
            _ => "manga",
        };
        let all = [&self.title.english, &self.title.romaji]
            .into_iter()
            .flatten()
            .chain(&self.synonyms);
        Some(Candidate {
            names: names(all),
            shown: Suggestion {
                title,
                kind,
                year: self.start.and_then(|d| d.year),
            },
            popularity: Some(self.popularity.unwrap_or(0)),
        })
    }
}

#[derive(Deserialize)]
struct MdList {
    data: Vec<MdManga>,
}

#[derive(Deserialize)]
struct MdManga {
    attributes: MdAttributes,
}

#[derive(Deserialize)]
struct MdAttributes {
    title: HashMap<String, String>,
    #[serde(rename = "altTitles", default)]
    alt_titles: Vec<HashMap<String, String>>,
    links: Option<HashMap<String, String>>,
    #[serde(rename = "originalLanguage")]
    language: Option<String>,
    year: Option<i32>,
}

impl MdAttributes {
    fn candidate(self) -> (Candidate, Option<u64>) {
        let alt_en = self.alt_titles.iter().find_map(|t| t.get("en"));
        let title = self
            .title
            .get("en")
            .or(alt_en)
            .or_else(|| self.title.values().next())
            .cloned()
            .unwrap_or_default();
        let kind = match self.language.as_deref() {
            Some("ko") => "manhwa",
            Some(l) if l.starts_with("zh") => "manhua",
            _ => "manga",
        };
        let all = self
            .title
            .values()
            .chain(self.alt_titles.iter().flat_map(|t| t.values()));
        let anilist = self
            .links
            .as_ref()
            .and_then(|l| l.get("al"))
            .and_then(|id| id.parse().ok());
        let candidate = Candidate {
            names: names(all),
            shown: Suggestion {
                title,
                kind,
                year: self.year,
            },
            popularity: None,
        };
        (candidate, anilist)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn al(
        id: u64,
        english: Option<&str>,
        romaji: &str,
        popularity: u32,
        country: &str,
    ) -> serde_json::Value {
        json!({ "id": id, "title": { "english": english, "romaji": romaji }, "synonyms": [],
                "popularity": popularity, "countryOfOrigin": country, "format": "MANGA",
                "startDate": { "year": 2018 } })
    }

    fn md(en: &str, anilist: Option<&str>, language: &str) -> serde_json::Value {
        json!({ "attributes": { "title": { "en": en }, "altTitles": [],
                "links": anilist.map(|id| json!({ "al": id })),
                "originalLanguage": language, "year": 2020 } })
    }

    async fn suggester(
        mangadex: ResponseTemplate,
        anilist: ResponseTemplate,
    ) -> (MockServer, Suggester) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/manga"))
            .and(query_param("title", "one"))
            .respond_with(mangadex)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(json!({ "variables": { "q": "one" } })))
            .respond_with(anilist)
            .mount(&server)
            .await;
        let s = Suggester::new(
            reqwest::Client::new(),
            &format!("{}/graphql", server.uri()),
            &server.uri(),
        );
        (server, s)
    }

    fn titles(found: &[Suggestion]) -> Vec<&str> {
        found.iter().map(|s| s.title.as_str()).collect()
    }

    #[tokio::test]
    async fn starts_with_first_then_popularity_and_mangadex_only_series_kept() {
        let mangadex = json!({ "data": [
            md("One Punch-Man", Some("1"), "ja"),
            md("The One Who Stayed", None, "ko"),
            md("Unrelated Fuzzy Match", None, "ja"),
        ] });
        let anilist = json!({ "data": {
            "found": { "media": [
                al(30013, Some("One Piece"), "ONE PIECE", 300, "JP"),
                al(2, None, "One Outs", 10, "JP"),
                al(3, Some("Kiichi!!"), "Kiichi", 50, "JP"),
            ] },
            "linked": { "media": [ al(1, Some("One-Punch Man"), "One Punch-Man", 200, "JP") ] }
        } });
        let (_server, s) = suggester(
            ResponseTemplate::new(200).set_body_json(mangadex),
            ResponseTemplate::new(200).set_body_json(anilist),
        )
        .await;
        let found = s.suggest("one").await.unwrap();
        assert_eq!(
            titles(&found),
            [
                "One Piece",
                "One-Punch Man",
                "One Outs",
                "The One Who Stayed"
            ],
            "Kiichi and the fuzzy match don't match \"one\"; One Punch-Man is listed once"
        );
        assert_eq!(found[3].kind, "manhwa");
    }

    #[tokio::test]
    async fn mangadex_still_suggests_while_anilist_rate_limits() {
        let mangadex = json!({ "data": [ md("One Piece", Some("30013"), "ja") ] });
        let (_server, s) = suggester(
            ResponseTemplate::new(200).set_body_json(mangadex),
            ResponseTemplate::new(429),
        )
        .await;
        assert_eq!(titles(&s.suggest("one").await.unwrap()), ["One Piece"]);
    }

    #[tokio::test]
    async fn both_failing_gives_anilists_error() {
        let (_server, s) = suggester(ResponseTemplate::new(500), ResponseTemplate::new(429)).await;
        let err = s.suggest("one").await.unwrap_err();
        assert!(format!("{err:#}").contains("rate limiting"), "{err:#}");
    }

    #[tokio::test]
    #[ignore]
    async fn live_smoke() {
        let s = Suggester::new(crate::sources::http_client(), ANILIST, MANGADEX);
        for (typed, expected) in [
            ("one", "One Piece"),
            ("kagu", "Kaguya-sama: Love is War"),
            ("frier", "Frieren"),
            ("the beginning after", "The Beginning After the End"),
        ] {
            let found = s.suggest(typed).await.unwrap();
            eprintln!("{typed:?}: {:?}", titles(&found));
            assert!(
                titles(&found).iter().any(|t| t.starts_with(expected)),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn tiers() {
        let n = |t: &[&str]| t.iter().map(|s| normalize_title(s)).collect::<Vec<_>>();
        assert_eq!(tier("kagu", &n(&["Kaguya-sama: Love is War"])), 0);
        assert_eq!(tier("love is", &n(&["Kaguya-sama: Love is War"])), 1);
        assert_eq!(tier("ove", &n(&["Kaguya-sama: Love is War"])), 2);
    }
}
