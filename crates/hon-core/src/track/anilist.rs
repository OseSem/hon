//! AniList over GraphQL, as a logged-in user.

use anyhow::{Context, anyhow, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ANILIST, Entry, Match, Status, TrackError, Tracker};

pub const ENDPOINT: &str = "https://graphql.anilist.co";
/// hon's AniList API client. Public: the login hands the token straight to the user.
pub const CLIENT_ID: &str = "58125";
/// Where logging in starts; AniList then shows the token to paste into hon.
pub const AUTHORIZE_URL: &str =
    "https://anilist.co/api/v2/oauth/authorize?client_id=58125&response_type=token";
/// Matches offered when linking a series.
const MATCHES: u32 = 5;

const SEARCH: &str = "query ($q: String, $n: Int) {
  Page(perPage: $n) {
    media(search: $q, type: MANGA, format_not_in: [NOVEL]) {
      id title { english romaji } format startDate { year } chapters
    }
  }
}";
const ENTRY: &str = "query ($id: Int) {
  Media(id: $id) { chapters mediaListEntry { progress status } }
}";
const SAVE: &str = "mutation ($id: Int, $p: Int, $s: MediaListStatus) {
  SaveMediaListEntry(mediaId: $id, progress: $p, status: $s) { id }
}";
const VIEWER: &str = "{ Viewer { id name } }";

pub struct AniList {
    http: reqwest::Client,
    endpoint: String,
    token: String,
}

impl AniList {
    pub fn new(http: reqwest::Client, endpoint: &str, token: &str) -> AniList {
        AniList {
            http,
            endpoint: endpoint.to_string(),
            token: token.to_string(),
        }
    }

    /// The logged-in user's id and name; checks the token.
    pub async fn viewer(&self) -> anyhow::Result<(i64, String)> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "Viewer")]
            viewer: Viewer,
        }
        #[derive(Deserialize)]
        struct Viewer {
            id: i64,
            name: String,
        }
        let data: Data = self.call(VIEWER, json!({})).await?;
        Ok((data.viewer.id, data.viewer.name))
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> anyhow::Result<T> {
        let res = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .context("reaching AniList")?;
        let status = res.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(TrackError::RateLimited.into());
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(TrackError::LoggedOut.into());
        }
        #[derive(Deserialize)]
        struct Reply {
            data: Option<Value>,
            #[serde(default)]
            errors: Vec<GraphError>,
        }
        #[derive(Deserialize)]
        struct GraphError {
            message: String,
        }
        let text = res.text().await.context("reading AniList's answer")?;
        let reply: Reply = serde_json::from_str(&text)
            .map_err(|_| anyhow!("AniList answered {status} with something hon can't read"))?;
        if let Some(e) = reply.errors.first() {
            // A bad or revoked token is a 400 "Invalid token".
            if e.message.to_lowercase().contains("token") || e.message == "Unauthorized." {
                return Err(TrackError::LoggedOut.into());
            }
            bail!("AniList: {}", e.message);
        }
        let data = reply
            .data
            .ok_or_else(|| anyhow!("AniList answered {status} without data"))?;
        serde_json::from_value(data).context("reading AniList's answer")
    }
}

#[derive(Deserialize)]
struct Title {
    english: Option<String>,
    romaji: Option<String>,
}

#[derive(Deserialize)]
struct Year {
    year: Option<i32>,
}

#[derive(Deserialize)]
struct Media {
    id: i64,
    title: Title,
    format: Option<String>,
    #[serde(rename = "startDate")]
    start_date: Option<Year>,
    chapters: Option<u32>,
}

fn status_of(s: &str) -> Status {
    match s {
        "CURRENT" => Status::Current,
        "COMPLETED" => Status::Completed,
        "REPEATING" => Status::Repeating,
        _ => Status::Other,
    }
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Current | Status::Other => "CURRENT",
        Status::Completed => "COMPLETED",
        Status::Repeating => "REPEATING",
    }
}

#[async_trait::async_trait]
impl Tracker for AniList {
    fn name(&self) -> &'static str {
        ANILIST
    }

    async fn search(&self, title: &str) -> anyhow::Result<Vec<Match>> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        struct Page {
            media: Vec<Media>,
        }
        let data: Data = self
            .call(SEARCH, json!({ "q": title, "n": MATCHES }))
            .await?;
        Ok(data
            .page
            .media
            .into_iter()
            .filter_map(|m| {
                Some(Match {
                    id: m.id,
                    title: m.title.english.or(m.title.romaji)?,
                    format: m.format,
                    year: m.start_date.and_then(|d| d.year),
                    chapters: m.chapters,
                })
            })
            .collect())
    }

    async fn entry(&self, id: i64) -> anyhow::Result<Entry> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "Media")]
            media: Found,
        }
        #[derive(Deserialize)]
        struct Found {
            chapters: Option<u32>,
            #[serde(rename = "mediaListEntry")]
            entry: Option<Listed>,
        }
        #[derive(Deserialize)]
        struct Listed {
            progress: Option<u32>,
            status: Option<String>,
        }
        let data: Data = self.call(ENTRY, json!({ "id": id })).await?;
        let listed = data.media.entry;
        Ok(Entry {
            progress: listed.as_ref().map(|l| l.progress.unwrap_or(0)),
            status: listed.and_then(|l| l.status).map(|s| status_of(&s)),
            chapters: data.media.chapters,
        })
    }

    async fn save(&self, id: i64, progress: u32, status: Status) -> anyhow::Result<()> {
        let variables = json!({ "id": id, "p": progress, "s": status_name(status) });
        let _: Value = self.call(SAVE, variables).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn anilist(reply: ResponseTemplate) -> (MockServer, AniList) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("Authorization", "Bearer tok"))
            .respond_with(reply)
            .mount(&server)
            .await;
        let client = AniList::new(reqwest::Client::new(), &server.uri(), "tok");
        (server, client)
    }

    fn ok(data: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
    }

    #[tokio::test]
    async fn search_prefers_english_titles() {
        let (_s, al) = anilist(ok(json!({ "Page": { "media": [
            { "id": 105398, "title": { "english": "Solo Leveling", "romaji": "Na Honjaman Level Up" },
              "format": "MANGA", "startDate": { "year": 2018 }, "chapters": 201 },
            { "id": 2, "title": { "english": null, "romaji": "Ore dake" },
              "format": "ONE_SHOT", "startDate": { "year": null }, "chapters": null },
        ] } })))
        .await;
        assert_eq!(
            al.search("solo leveling").await.unwrap(),
            [
                Match {
                    id: 105398,
                    title: "Solo Leveling".into(),
                    format: Some("MANGA".into()),
                    year: Some(2018),
                    chapters: Some(201),
                },
                Match {
                    id: 2,
                    title: "Ore dake".into(),
                    format: Some("ONE_SHOT".into()),
                    year: None,
                    chapters: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn entry_on_and_off_the_list() {
        let (_s, al) = anilist(ok(
            json!({ "Media": { "chapters": 201, "mediaListEntry": null } }),
        ))
        .await;
        assert_eq!(
            al.entry(1).await.unwrap(),
            Entry {
                progress: None,
                status: None,
                chapters: Some(201)
            }
        );
        let (_s, al) = anilist(ok(json!({ "Media": { "chapters": null,
            "mediaListEntry": { "progress": 40, "status": "PAUSED" } } })))
        .await;
        assert_eq!(
            al.entry(1).await.unwrap(),
            Entry {
                progress: Some(40),
                status: Some(Status::Other),
                chapters: None
            }
        );
    }

    #[tokio::test]
    async fn save_sends_progress_and_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({ "variables": { "id": 7, "p": 12, "s": "COMPLETED" } }),
            ))
            .respond_with(ok(json!({ "SaveMediaListEntry": { "id": 1 } })))
            .expect(1)
            .mount(&server)
            .await;
        let al = AniList::new(reqwest::Client::new(), &server.uri(), "tok");
        al.save(7, 12, Status::Completed).await.unwrap();
    }

    #[tokio::test]
    async fn bad_token_and_rate_limit_are_typed() {
        let invalid = ResponseTemplate::new(400).set_body_json(
            json!({ "data": null, "errors": [{ "message": "Invalid token", "status": 400 }] }),
        );
        let (_s, al) = anilist(invalid).await;
        let e = al.viewer().await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&TrackError::LoggedOut));

        let (_s, al) = anilist(ResponseTemplate::new(401)).await;
        let e = al.entry(1).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&TrackError::LoggedOut));

        let (_s, al) = anilist(ResponseTemplate::new(429)).await;
        let e = al.save(1, 1, Status::Current).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&TrackError::RateLimited));

        let (_s, al) = anilist(ResponseTemplate::new(500).set_body_string("oops")).await;
        let e = al.search("x").await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "AniList answered 500 Internal Server Error with something hon can't read"
        );
        assert!(e.downcast_ref::<TrackError>().is_none());

        let not_found = ResponseTemplate::new(404).set_body_json(
            json!({ "data": { "Media": null }, "errors": [{ "message": "Not Found." }] }),
        );
        let (_s, al) = anilist(not_found).await;
        assert_eq!(
            al.entry(1).await.unwrap_err().to_string(),
            "AniList: Not Found."
        );
    }

    #[tokio::test]
    async fn viewer_reads_id_and_name() {
        let (_s, al) = anilist(ok(json!({ "Viewer": { "id": 8, "name": "sem" } }))).await;
        assert_eq!(al.viewer().await.unwrap(), (8, "sem".to_string()));
    }

    #[test]
    fn authorize_url_uses_the_client_id() {
        assert!(AUTHORIZE_URL.contains(&format!("client_id={CLIENT_ID}&")));
    }
}
