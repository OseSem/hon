//! `hon login`, `hon logout`, `hon track`, and keeping reading progress in step with AniList
//! while reading.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::sync::mpsc::UnboundedReceiver;

use super::{Ctx, all_chapters, need_sources, resolve_source, resolve_title};
use crate::config::Paths;
use hon_core::db::{Db, Link, Series};
use hon_core::search::{TitleGroup, search_all};
use hon_core::sources::http_client;
use hon_core::track::anilist::{AUTHORIZE_URL, AniList, ENDPOINT};
use hon_core::track::login::{self, Login};
use hon_core::track::sync::Syncer;
use hon_core::track::{
    ANILIST, Match, TrackError, Tracker, apply_remote, local_number, queue_local,
};

/// How long opening a series waits for AniList before going on without it.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long hon waits for queued progress to go out before exiting.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(3);

/// A logged-in tracker: the login, the client and the task sending queued progress.
pub struct Tracking {
    pub login: Login,
    pub tracker: Arc<dyn Tracker>,
    pub syncer: Syncer,
    /// Failures the syncer reports; whoever shows them takes them.
    pub notes: Mutex<UnboundedReceiver<String>>,
}

impl Tracking {
    /// The saved login's tracking, sending anything left queued; `None` when not logged in.
    /// Must run inside the tokio runtime.
    pub fn start(paths: &Paths) -> anyhow::Result<Option<Tracking>> {
        let Some(login) = login::load(&paths.tracking_file)? else {
            return Ok(None);
        };
        let tracker: Arc<dyn Tracker> =
            Arc::new(AniList::new(http_client(), ENDPOINT, &login.token));
        Ok(Some(Tracking::with(login, tracker, &paths.db_file)))
    }

    pub fn with(login: Login, tracker: Arc<dyn Tracker>, db_file: &Path) -> Tracking {
        let (syncer, notes) =
            Syncer::spawn(db_file.to_path_buf(), tracker.clone(), login.account());
        syncer.flush();
        Tracking {
            login,
            tracker,
            syncer,
            notes: Mutex::new(notes),
        }
    }

    pub fn account(&self) -> String {
        self.login.account()
    }

    /// The syncer's notes so far.
    pub fn take_notes(&self) -> Vec<String> {
        let mut notes = self.notes.lock().unwrap_or_else(|p| p.into_inner());
        std::iter::from_fn(|| notes.try_recv().ok()).collect()
    }

    /// Queues the series' progress when it's linked and ahead of AniList, and sends it.
    pub fn read(&self, db: &Db, series: i64) -> anyhow::Result<()> {
        if queue_local(db, ANILIST, &self.account(), series)? {
            self.syncer.flush();
        }
        Ok(())
    }

    /// Waits a moment for queued progress to go out; returns the notes to show.
    pub async fn finish(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.syncer.finish(FINISH_TIMEOUT).await {
            notes.push("AniList is slow; the rest of your progress goes out next time".into());
        }
        let mut taken = self.take_notes();
        taken.extend(notes);
        taken
    }
}

impl Ctx {
    /// After chapters of `series` were marked read: queues and sends them to AniList.
    pub fn tracked_read(&self, series: i64) -> anyhow::Result<()> {
        match &self.tracking {
            Some(t) => t.read(&self.db, series),
            None => Ok(()),
        }
    }

    /// Prints what tracking has to say before hon exits.
    pub async fn finish_tracking(&self) {
        if let Some(t) = &self.tracking {
            for note in t.finish().await {
                eprintln!("warning: {note}");
            }
        }
    }
}

/// What the user picked for a series.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    Link(Match),
    /// Don't track the series; not asked again.
    DontTrack,
    /// Leave it as it is.
    Keep,
}

/// Asks which of `matches` the series titled `title` is, given its current link.
pub type Choose<'a> = &'a mut dyn FnMut(&str, &[Match], Option<&Link>) -> anyhow::Result<Choice>;

/// One line for an AniList match: title, kind, year and chapter count.
pub fn match_line(m: &Match) -> String {
    let mut parts = vec![m.title.clone()];
    if let Some(format) = &m.format {
        parts.push(format.to_lowercase().replace('_', "-"));
    }
    if let Some(year) = m.year {
        parts.push(year.to_string());
    }
    if let Some(chapters) = m.chapters {
        parts.push(format!("{chapters} ch"));
    }
    parts.join(" · ")
}

/// The numbered list on the terminal. Enter keeps the current link, or doesn't track a
/// series that has none.
pub fn choose_on_terminal(
    input: &mut impl BufRead,
    out: &mut impl Write,
    title: &str,
    matches: &[Match],
    current: Option<&Link>,
) -> anyhow::Result<Choice> {
    writeln!(out, "Which AniList entry is \"{title}\"?")?;
    for (i, m) in matches.iter().enumerate() {
        writeln!(out, "  {}  {}", i + 1, match_line(m))?;
    }
    writeln!(out, "  0  don't track it")?;
    let linked = current.and_then(|l| l.remote_id.and(l.title.clone()));
    let default = match &linked {
        Some(title) => format!("keep {title}"),
        None => "don't track it".to_string(),
    };
    loop {
        write!(out, "Number (Enter: {default}): ")?;
        out.flush()?;
        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            return Ok(Choice::Keep);
        }
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(if linked.is_some() {
                Choice::Keep
            } else {
                Choice::DontTrack
            });
        }
        match answer.parse::<usize>() {
            Ok(0) => return Ok(Choice::DontTrack),
            Ok(i) if i <= matches.len() => return Ok(Choice::Link(matches[i - 1].clone())),
            _ => writeln!(out, "pick a number from 0 to {}", matches.len())?,
        }
    }
}

/// Stores `choice` for the series titled `title`; returns what to tell the user.
pub fn record(
    db: &Db,
    t: &Tracking,
    series: i64,
    title: &str,
    choice: &Choice,
) -> anyhow::Result<String> {
    let account = t.account();
    Ok(match choice {
        Choice::Link(m) => {
            db.set_link(series, ANILIST, &account, Some(m.id), Some(&m.title))?;
            format!("tracking \"{title}\" on AniList as {}", m.title)
        }
        Choice::DontTrack => {
            db.set_link(series, ANILIST, &account, None, None)?;
            format!("not tracking \"{title}\" on AniList (`hon track` changes that)")
        }
        Choice::Keep => String::new(),
    })
}

/// Takes in AniList's number `remote` for entry `id`: marks `chapters` up to it read when
/// it's ahead, and queues hon's number when hon is.
pub fn take_in(
    db: &Db,
    t: &Tracking,
    series: i64,
    chapters: &[hon_core::model::ChapterNo],
    id: i64,
    remote: u32,
) -> anyhow::Result<()> {
    apply_remote(db, series, chapters, remote)?;
    db.set_synced(series, ANILIST, &t.account(), id, remote)?;
    if local_number(&db.progress(series)?) > remote {
        t.read(db, series)?;
    }
    Ok(())
}

/// When a series opens: asks which AniList entry it is the first time (when `choose` is
/// given), then takes in AniList's progress if it's ahead and queues hon's if hon is.
/// Network trouble comes back as a note, never as an error.
pub async fn link_and_pull(
    ctx: &Ctx,
    group: &TitleGroup,
    series: &Series,
    choose: Option<Choose<'_>>,
) -> anyhow::Result<Vec<String>> {
    let Some(t) = &ctx.tracking else {
        return Ok(Vec::new());
    };
    let mut notes = Vec::new();
    let mut link = ctx.db.link(series.id, ANILIST, &t.account())?;
    if link.is_none() {
        let Some(choose) = choose else {
            return Ok(notes);
        };
        let matches = match with_timeout(t.tracker.search(&group.title)).await {
            Ok(matches) => matches,
            Err(e) => return Ok(vec![note(&e)]),
        };
        if matches.is_empty() {
            return Ok(vec![format!(
                "AniList has nothing called \"{}\"; `hon track` asks again",
                group.title
            )]);
        }
        let choice = choose(&group.title, &matches, None)?;
        let said = record(&ctx.db, t, series.id, &series.title, &choice)?;
        if !said.is_empty() {
            notes.push(said);
        }
        link = ctx.db.link(series.id, ANILIST, &t.account())?;
    }
    if let Some(note) = pull(ctx, t, group, series, link.as_ref()).await? {
        notes.push(note);
    }
    Ok(notes)
}

/// Takes in AniList's number for a linked series when it's ahead, and queues hon's when
/// hon is. `Some` note when AniList couldn't be asked.
async fn pull(
    ctx: &Ctx,
    t: &Tracking,
    group: &TitleGroup,
    series: &Series,
    link: Option<&Link>,
) -> anyhow::Result<Option<String>> {
    let Some(id) = link.and_then(|l| l.remote_id) else {
        return Ok(None);
    };
    let entry = match with_timeout(t.tracker.entry(id)).await {
        Ok(entry) => entry,
        Err(e) => return Ok(Some(note(&e))),
    };
    let remote = entry.progress.unwrap_or(0);
    take_in(&ctx.db, t, series.id, &all_chapters(group), id, remote)?;
    Ok(None)
}

pub async fn with_timeout<T>(call: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(ASK_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| bail!("AniList didn't answer in time"))
}

/// A one-line note for an AniList failure.
pub fn note(e: &anyhow::Error) -> String {
    match e.downcast_ref::<TrackError>() {
        Some(e) => e.to_string(),
        None => format!("couldn't reach AniList: {e:#}"),
    }
}

/// `hon login anilist`: opens AniList's approval page and saves the token pasted back.
pub async fn login(
    paths: &Paths,
    endpoint: &str,
    input: &mut impl BufRead,
    out: &mut impl Write,
    open: impl FnOnce(&str),
) -> anyhow::Result<()> {
    writeln!(
        out,
        "Approve hon on AniList, then paste the token it shows.\n\
         If no browser opens, go to:\n  {AUTHORIZE_URL}"
    )?;
    open(AUTHORIZE_URL);
    write!(out, "Token: ")?;
    out.flush()?;
    let mut token = String::new();
    input.read_line(&mut token)?;
    let token = token.trim();
    if token.is_empty() {
        bail!("no token pasted; nothing changed");
    }
    let client = AniList::new(http_client(), endpoint, token);
    let (user_id, name) = client.viewer().await.map_err(|e| match e.downcast_ref() {
        Some(TrackError::LoggedOut) => anyhow::anyhow!("AniList didn't accept that token"),
        _ => e,
    })?;
    let login = Login {
        token: token.to_string(),
        user_id,
        name: name.clone(),
    };
    login::save(&paths.tracking_file, &login)?;
    writeln!(out, "Logged in to AniList as {name}")?;
    Ok(())
}

/// `hon logout anilist`.
pub fn logout(paths: &Paths, out: &mut impl Write) -> anyhow::Result<()> {
    if login::remove(&paths.tracking_file)? {
        writeln!(out, "Logged out of AniList")?;
    } else {
        writeln!(out, "You weren't logged in to AniList")?;
    }
    Ok(())
}

/// `hon track <title>`: picks, changes or drops the series' AniList entry.
pub async fn track(
    ctx: &Ctx,
    title: &str,
    source: Option<&str>,
    choose: Choose<'_>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let Some(t) = &ctx.tracking else {
        bail!("not logged in to AniList; run `hon login anilist` first");
    };
    need_sources(ctx)?;
    let source = source.map(|s| resolve_source(ctx, s)).transpose()?;
    let outcome = search_all(&ctx.sources, title).await;
    let (group, _) = resolve_title(ctx, title, source.as_ref(), &outcome)?;
    let series = ctx.db.series(&group.keys, &group.title)?;
    let current = ctx.db.link(series.id, ANILIST, &t.account())?;
    let matches = with_timeout(t.tracker.search(&group.title))
        .await
        .context("searching AniList")?;
    let choice = choose(&group.title, &matches, current.as_ref())?;
    let said = record(&ctx.db, t, series.id, &series.title, &choice)?;
    if !said.is_empty() {
        writeln!(out, "{said}")?;
    }
    let link = ctx.db.link(series.id, ANILIST, &t.account())?;
    if let Some(note) = pull(ctx, t, group, &series, link.as_ref()).await? {
        writeln!(out, "warning: {note}")?;
    }
    Ok(())
}

/// The terminal chooser, reading stdin and writing stdout.
pub fn terminal_chooser() -> impl FnMut(&str, &[Match], Option<&Link>) -> anyhow::Result<Choice> {
    |title: &str, matches: &[Match], current: Option<&Link>| {
        choose_on_terminal(
            &mut io::stdin().lock(),
            &mut io::stdout(),
            title,
            matches,
            current,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tests::{Fake as FakeSource, ctx as ctx_with, no};
    use hon_core::db::Link;
    use hon_core::model::test_ids::alpha;
    use hon_core::track::fake::Fake;
    use hon_core::track::{Entry, Status};

    fn m(id: i64, title: &str) -> Match {
        Match {
            id,
            title: title.into(),
            format: Some("ONE_SHOT".into()),
            year: Some(2018),
            chapters: Some(201),
        }
    }

    #[test]
    fn match_lines() {
        assert_eq!(match_line(&m(1, "Solo")), "Solo · one-shot · 2018 · 201 ch");
        let bare = Match {
            format: None,
            year: None,
            chapters: None,
            ..m(1, "Solo")
        };
        assert_eq!(match_line(&bare), "Solo");
    }

    fn choose(input: &str, current: Option<&Link>) -> (Choice, String) {
        let mut out = Vec::new();
        let matches = [m(1, "A"), m(2, "B")];
        let choice =
            choose_on_terminal(&mut input.as_bytes(), &mut out, "T", &matches, current).unwrap();
        (choice, String::from_utf8(out).unwrap())
    }

    #[test]
    fn terminal_choice() {
        let (choice, out) = choose("2\n", None);
        assert_eq!(choice, Choice::Link(m(2, "B")));
        assert!(
            out.starts_with("Which AniList entry is \"T\"?\n  1  A ·"),
            "{out}"
        );
        assert!(out.contains("  0  don't track it\nNumber (Enter: don't track it): "));
        assert_eq!(choose("\n", None).0, Choice::DontTrack);
        assert_eq!(choose("0\n", None).0, Choice::DontTrack);
        let (choice, out) = choose("9\nx\n1\n", None);
        assert_eq!(choice, Choice::Link(m(1, "A")));
        assert_eq!(out.matches("pick a number from 0 to 2").count(), 2);
        assert_eq!(choose("", None).0, Choice::Keep, "stdin closed");

        let linked = Link {
            remote_id: Some(1),
            title: Some("A".into()),
            synced: None,
        };
        let (choice, out) = choose("\n", Some(&linked));
        assert_eq!(choice, Choice::Keep);
        assert!(out.contains("(Enter: keep A)"), "{out}");
    }

    struct Setup {
        _dir: tempfile::TempDir,
        ctx: Ctx,
        fake: Arc<Fake>,
        group: TitleGroup,
    }

    async fn setup(fake: Fake) -> Setup {
        let source = FakeSource::new(
            alpha(),
            &["Solo Leveling"],
            (1..=5).map(|n| n.to_string()).collect(),
        );
        let (dir, mut ctx) = ctx_with(vec![Arc::new(source)]);
        let fake = Arc::new(fake);
        let login = Login {
            token: "t".into(),
            user_id: 8,
            name: "sem".into(),
        };
        ctx.tracking = Some(Tracking::with(
            login,
            fake.clone(),
            &dir.path().join("hon.db"),
        ));
        let outcome = search_all(&ctx.sources, "solo leveling").await;
        let group = outcome.groups[0].clone();
        Setup {
            _dir: dir,
            ctx,
            fake,
            group,
        }
    }

    fn series(s: &Setup) -> Series {
        s.ctx.db.series(&s.group.keys, &s.group.title).unwrap()
    }

    #[tokio::test]
    async fn first_open_asks_links_and_takes_in_anilist() {
        let fake = Fake {
            matches: vec![m(105398, "Solo Leveling")],
            ..Fake::default()
        };
        fake.entries.lock().unwrap().insert(
            105398,
            Entry {
                progress: Some(3),
                status: Some(Status::Current),
                chapters: None,
            },
        );
        let s = setup(fake).await;
        let series = series(&s);
        let mut asked = 0;
        let mut pick = |_: &str, matches: &[Match], current: Option<&Link>| {
            asked += 1;
            assert!(current.is_none());
            Ok(Choice::Link(matches[0].clone()))
        };
        let notes = link_and_pull(&s.ctx, &s.group, &series, Some(&mut pick))
            .await
            .unwrap();
        assert_eq!(
            notes,
            ["tracking \"Solo Leveling\" on AniList as Solo Leveling"]
        );
        let progress = s.ctx.db.progress(series.id).unwrap();
        assert!((1..=3).all(|n| progress[&no(&n.to_string())].read));
        assert!(!progress.contains_key(&no("4")));

        // Linked now: not asked again.
        let notes = link_and_pull(&s.ctx, &s.group, &series, Some(&mut pick))
            .await
            .unwrap();
        assert!(notes.is_empty());
        assert_eq!(asked, 1);
        assert!(s.fake.saves.lock().unwrap().is_empty(), "AniList was ahead");
    }

    #[tokio::test]
    async fn hon_ahead_is_queued_and_sent() {
        let s = setup(Fake {
            matches: vec![m(7, "Solo Leveling")],
            ..Fake::default()
        })
        .await;
        let series = series(&s);
        s.ctx.db.set_read(series.id, no("4"), true).unwrap();
        let mut pick = |_: &str, m: &[Match], _: Option<&Link>| Ok(Choice::Link(m[0].clone()));
        link_and_pull(&s.ctx, &s.group, &series, Some(&mut pick))
            .await
            .unwrap();
        let t = s.ctx.tracking.as_ref().unwrap();
        assert!(t.finish().await.is_empty());
        assert_eq!(*s.fake.saves.lock().unwrap(), [(7, 4, Status::Current)]);

        // Reading on queues the new number; reading nothing new sends nothing.
        s.ctx.db.set_read(series.id, no("5"), true).unwrap();
        s.ctx.tracked_read(series.id).unwrap();
        s.ctx.tracked_read(series.id).unwrap();
        assert!(t.finish().await.is_empty());
        assert_eq!(s.fake.saves.lock().unwrap().len(), 2);
        assert_eq!(s.fake.saves.lock().unwrap()[1], (7, 5, Status::Current));
    }

    #[tokio::test]
    async fn skipped_and_unasked_series_stay_untracked() {
        let s = setup(Fake {
            matches: vec![m(7, "Solo Leveling")],
            ..Fake::default()
        })
        .await;
        let series = series(&s);
        // Without a chooser nothing is asked or recorded.
        assert!(
            link_and_pull(&s.ctx, &s.group, &series, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.ctx.db.link(series.id, ANILIST, "8").unwrap(), None);

        let mut skip = |_: &str, _: &[Match], _: Option<&Link>| Ok(Choice::DontTrack);
        let notes = link_and_pull(&s.ctx, &s.group, &series, Some(&mut skip))
            .await
            .unwrap();
        assert_eq!(
            notes,
            ["not tracking \"Solo Leveling\" on AniList (`hon track` changes that)"]
        );
        let mut never = |_: &str, _: &[Match], _: Option<&Link>| -> anyhow::Result<Choice> {
            panic!("asked again")
        };
        s.ctx.db.set_read(series.id, no("2"), true).unwrap();
        s.ctx.tracked_read(series.id).unwrap();
        link_and_pull(&s.ctx, &s.group, &series, Some(&mut never))
            .await
            .unwrap();
        assert!(s.ctx.tracking.as_ref().unwrap().finish().await.is_empty());
        assert!(s.fake.saves.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn anilist_down_is_a_note() {
        let s = setup(Fake {
            matches: vec![m(7, "Solo Leveling")],
            ..Fake::default()
        })
        .await;
        let series = series(&s);
        s.ctx
            .db
            .set_link(series.id, ANILIST, "8", Some(7), None)
            .unwrap();
        *s.fake.fail.lock().unwrap() = Some(TrackError::LoggedOut);
        let notes = link_and_pull(&s.ctx, &s.group, &series, None)
            .await
            .unwrap();
        assert_eq!(notes, ["AniList login expired; run `hon login anilist`"]);
    }

    #[tokio::test]
    async fn logout_and_bad_login() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let mut out = Vec::new();
        logout(&paths, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "You weren't logged in to AniList\n"
        );

        let mut opened = None;
        let mut out = Vec::new();
        let e = login(
            &paths,
            "http://127.0.0.1:9",
            &mut "\n".as_bytes(),
            &mut out,
            |u| opened = Some(u.to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "no token pasted; nothing changed");
        assert_eq!(opened.as_deref(), Some(AUTHORIZE_URL));
        assert!(String::from_utf8(out).unwrap().contains(AUTHORIZE_URL));
    }

    #[tokio::test]
    async fn login_saves_the_checked_token() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": { "Viewer": { "id": 8, "name": "sem" } } }),
            ))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let mut out = Vec::new();
        login(
            &paths,
            &server.uri(),
            &mut " tok \n".as_bytes(),
            &mut out,
            |_| {},
        )
        .await
        .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .ends_with("Logged in to AniList as sem\n")
        );
        let saved = login::load(&paths.tracking_file).unwrap().unwrap();
        assert_eq!((saved.token.as_str(), saved.user_id), ("tok", 8));
        let mut out = Vec::new();
        logout(&paths, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "Logged out of AniList\n");
    }
}
