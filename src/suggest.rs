//! Title suggestions while typing a search, from AniList's catalogue.

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

pub const ANILIST: &str = "https://graphql.anilist.co";
const SHOWN: u32 = 8;
const QUERY: &str = "query ($q: String, $n: Int) {
  Page(perPage: $n) {
    media(search: $q, type: MANGA, sort: SEARCH_MATCH, format_not_in: [NOVEL]) {
      title { english romaji }
      countryOfOrigin
      format
      startDate { year }
    }
  }
}";

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub title: String,
    /// manga, manhwa, manhua or one-shot.
    pub kind: &'static str,
    pub year: Option<i32>,
}

pub struct AniList {
    http: reqwest::Client,
    endpoint: String,
}

impl AniList {
    pub fn new(http: reqwest::Client, endpoint: &str) -> AniList {
        AniList {
            http,
            endpoint: endpoint.to_string(),
        }
    }

    pub async fn suggest(&self, prefix: &str) -> anyhow::Result<Vec<Suggestion>> {
        let res = self
            .http
            .post(&self.endpoint)
            .header("Accept", "application/json")
            .json(&json!({ "query": QUERY, "variables": { "q": prefix, "n": SHOWN } }))
            .send()
            .await
            .context("AniList")?;
        if res.status() == StatusCode::TOO_MANY_REQUESTS {
            bail!("AniList is rate limiting; slow down a moment");
        }
        let body: Response = res
            .error_for_status()
            .context("AniList")?
            .json()
            .await
            .context("AniList's answer")?;
        let mut out: Vec<Suggestion> = Vec::new();
        for m in body.data.page.media {
            let Some(title) = m.title.english.or(m.title.romaji) else {
                continue;
            };
            if out.iter().any(|s| s.title.eq_ignore_ascii_case(&title)) {
                continue;
            }
            let kind = match (m.format.as_deref(), m.country.as_deref()) {
                (Some("ONE_SHOT"), _) => "one-shot",
                (_, Some("KR")) => "manhwa",
                (_, Some("CN" | "TW")) => "manhua",
                _ => "manga",
            };
            out.push(Suggestion {
                title,
                kind,
                year: m.start.and_then(|d| d.year),
            });
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct Response {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    #[serde(rename = "Page")]
    page: Page,
}

#[derive(Deserialize)]
struct Page {
    media: Vec<Media>,
}

#[derive(Deserialize)]
struct Media {
    title: Title,
    #[serde(rename = "countryOfOrigin")]
    country: Option<String>,
    format: Option<String>,
    #[serde(rename = "startDate")]
    start: Option<FuzzyDate>,
}

#[derive(Deserialize)]
struct Title {
    english: Option<String>,
    romaji: Option<String>,
}

#[derive(Deserialize)]
struct FuzzyDate {
    year: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn anilist(response: ResponseTemplate) -> (MockServer, AniList) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({ "variables": { "q": "solo lev" } }),
            ))
            .respond_with(response)
            .mount(&server)
            .await;
        let client = AniList::new(reqwest::Client::new(), &server.uri());
        (server, client)
    }

    #[tokio::test]
    async fn english_title_first_then_romaji_with_kind_and_year() {
        let body = json!({ "data": { "Page": { "media": [
            { "title": { "english": "Solo Leveling", "romaji": "Na Honjaman Level Up" },
              "countryOfOrigin": "KR", "format": "MANGA", "startDate": { "year": 2018 } },
            { "title": { "english": null, "romaji": "Ore dake Level Up na Ken" },
              "countryOfOrigin": "JP", "format": "MANGA", "startDate": { "year": null } },
            { "title": { "english": "solo leveling", "romaji": "dup" },
              "countryOfOrigin": "KR", "format": "MANGA", "startDate": null },
            { "title": { "english": "Leveling Side Story", "romaji": null },
              "countryOfOrigin": "CN", "format": "ONE_SHOT", "startDate": { "year": 2020 } }
        ] } } });
        let (_server, client) = anilist(ResponseTemplate::new(200).set_body_json(body)).await;
        let got = client.suggest("solo lev").await.unwrap();
        assert_eq!(
            got,
            [
                Suggestion {
                    title: "Solo Leveling".into(),
                    kind: "manhwa",
                    year: Some(2018)
                },
                Suggestion {
                    title: "Ore dake Level Up na Ken".into(),
                    kind: "manga",
                    year: None
                },
                Suggestion {
                    title: "Leveling Side Story".into(),
                    kind: "one-shot",
                    year: Some(2020)
                },
            ]
        );
    }

    #[tokio::test]
    async fn rate_limit_says_so() {
        let (_server, client) = anilist(ResponseTemplate::new(429)).await;
        let err = client.suggest("solo lev").await.unwrap_err();
        assert!(format!("{err:#}").contains("rate limiting"), "{err:#}");
    }
}
