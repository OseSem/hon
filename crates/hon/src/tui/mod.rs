pub mod app;
pub mod ui;

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, anyhow, bail};
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use image::DynamicImage;
use ratatui::DefaultTerminal;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::time::MissedTickBehavior;

use crate::commands::track::{self, Choice, Tracking};
use crate::commands::{Ctx, FOLLOW_INTERVAL, all_chapters, import_opencomic, source_name_width};
use crate::config::Config;
use crate::viewer::{open_chapter, opencomic};
use app::{App, AppEvent, Effect};
use hon_core::db::{Db, Series};
use hon_core::download::{chapter_path, existing_download, fetch_pages, write_chapter};
use hon_core::model::{ChapterNo, ChapterRef, PageRef, SourceId};
use hon_core::search::{TitleGroup, search_all};
use hon_core::sources::{Source, http_client};
use hon_core::suggest::{ANILIST, MANGADEX, Suggester};
use hon_core::track::ANILIST as TRACKER;
use ui::PageView;

pub enum Start {
    Search,
    Read {
        group: TitleGroup,
        source: SourceId,
        chapter: Option<ChapterNo>,
    },
}

/// Fails unless stdout is a terminal; call before touching it.
pub fn ensure_terminal() -> anyhow::Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("hon needs an interactive terminal (try `hon search`)");
    }
    Ok(())
}

pub async fn run(ctx: Ctx, start: Start) -> anyhow::Result<()> {
    ensure_terminal()?;
    // Enters raw mode and the alternate screen, and installs a panic hook that
    // restores the terminal before the panic message is printed.
    let mut terminal = ratatui::try_init().inspect_err(|_| ratatui::restore())?;
    // Asks the terminal which image protocol and font size it has; must happen before
    // the event stream starts reading stdin.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let result = event_loop(&mut terminal, ctx, start, picker).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    ctx: Ctx,
    start: Start,
    picker: Picker,
) -> anyhow::Result<()> {
    let reader_mode = ctx.config.reader;
    let viewer_reports_progress = opencomic::is_opencomic(ctx.config.viewer.as_deref());
    let (tx, mut results) = unbounded_channel();
    let mut runtime = Runtime::new(ctx, tx);
    // What the AniList syncer reports goes to the status line.
    let mut notes = runtime.tracking.as_ref().map_or_else(
        || unbounded_channel().1,
        |t| {
            let mut taken = t.notes.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::replace(&mut *taken, unbounded_channel().1)
        },
    );
    let (encoded_tx, mut encoded) = unbounded_channel();
    let mut view = PageView::new(picker, encoded_tx);
    let mut terminal_events = EventStream::new();
    let mut follow_tick = tokio::time::interval(FOLLOW_INTERVAL);
    follow_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let (mut app, mut effects) = App::new(start, reader_mode);
    app.viewer_reports_progress = viewer_reports_progress;
    app.suggest = runtime.config.suggestions;
    app.source_name_width = source_name_width(&runtime.sources);
    app.has_sources = !runtime.sources.is_empty();
    app.tracking = runtime.tracking.is_some();
    loop {
        // DB effects run right here and answer with events, which may ask for more effects.
        while !effects.is_empty() {
            let mut events = Vec::new();
            for effect in std::mem::take(&mut effects) {
                if effect == Effect::Quit {
                    runtime.finish_tracking().await;
                    return Ok(());
                }
                events.extend(runtime.execute(effect, &app));
            }
            for ev in events {
                effects.extend(app.update(ev));
            }
        }
        terminal.draw(|f| ui::draw(f, &mut app, &mut view))?;
        let ev = tokio::select! {
            Some(done) = results.recv() => match runtime.current(done) {
                Some(ev) => ev,
                None => continue,
            },
            Some(page) = encoded.recv() => {
                view.finished(page);
                continue;
            }
            Some(note) = notes.recv() => AppEvent::Error(note),
            _ = follow_tick.tick(), if viewer_reports_progress => {
                match runtime.follow_viewer(&app) {
                    Some(ev) => ev,
                    None => continue,
                }
            }
            ev = terminal_events.next() => match ev {
                Some(Ok(Event::Key(key))) => AppEvent::Key(key),
                Some(Ok(Event::Resize(w, h))) => AppEvent::Resize(w, h),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
                None => {
                    runtime.finish_tracking().await;
                    return Ok(());
                }
            },
        };
        effects = app.update(ev);
    }
}

/// Carries out effects. The database stays on the event loop's task; only sources,
/// config values and owned data move into spawned tasks.
struct Runtime {
    sources: Vec<Arc<dyn Source>>,
    db: Db,
    config: Config,
    tx: UnboundedSender<Done>,
    /// Series id and canonical title of the open title group.
    series: Option<(i64, String)>,
    /// Bumped by every search, so only the latest one's results are shown.
    search: u64,
    /// The running search; a newer one cancels it, so its plugin calls don't hold up the
    /// new one's.
    searching: Option<tokio::task::JoinHandle<()>>,
    /// Bumped by every `ListPages`, so pages only land in the chapter they were fetched for.
    reading: u64,
    suggester: Arc<Suggester>,
    /// The pending suggestion request; a newer prefix cancels it.
    suggesting: Option<tokio::task::JoinHandle<()>>,
    /// OpenComic's progress being followed, once a chapter was opened in it.
    following: Option<Following>,
    /// Set while logged in to AniList.
    tracking: Option<Tracking>,
}

/// Following OpenComic for the open series, like `hon read` does.
struct Following {
    series: i64,
    /// When the chapter was opened, in Unix milliseconds; older progress is ignored.
    since_ms: i64,
    /// The progress file's mtime at the last good look; `None` until there is one.
    seen: Option<Option<SystemTime>>,
    /// The last error shown, so a lasting one isn't shown on every look.
    warning: Option<String>,
}

/// Typing pauses this long before titles are asked for: AniList allows 30 requests a minute.
const SUGGEST_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// A spawned effect's result, tagged with what it belongs to. The event loop drops it
/// when that is no longer current: checking there, where `Runtime` changes, can't race.
struct Done {
    event: AppEvent,
    search: Option<u64>,
    series: Option<i64>,
    reading: Option<u64>,
}

impl Done {
    fn untagged(event: AppEvent) -> Done {
        Done {
            event,
            search: None,
            series: None,
            reading: None,
        }
    }
}

impl Runtime {
    fn new(ctx: Ctx, tx: UnboundedSender<Done>) -> Runtime {
        Runtime {
            sources: ctx.sources,
            db: ctx.db,
            config: ctx.config,
            tx,
            series: None,
            search: 0,
            searching: None,
            reading: 0,
            suggester: Arc::new(Suggester::new(http_client(), ANILIST, MANGADEX)),
            suggesting: None,
            following: None,
            tracking: ctx.tracking,
        }
    }

    /// The event, unless it answers a superseded search, a series that's no longer open,
    /// or a chapter that's no longer being read.
    fn current(&self, done: Done) -> Option<AppEvent> {
        let series = self.series.as_ref().map(|(id, _)| *id);
        let search_ok = done.search.is_none_or(|g| g == self.search);
        let series_ok = done.series.is_none_or(|id| Some(id) == series);
        let reading_ok = done.reading.is_none_or(|g| g == self.reading);
        (search_ok && series_ok && reading_ok).then_some(done.event)
    }

    fn execute(&mut self, effect: Effect, app: &App) -> Vec<AppEvent> {
        match effect {
            Effect::Quit => Vec::new(),
            Effect::Search(query) => {
                self.search += 1;
                if let Some(pending) = self.searching.take() {
                    pending.abort();
                }
                let (sources, tx, search) = (self.sources.clone(), self.tx.clone(), self.search);
                self.searching = Some(tokio::spawn(async move {
                    let _ = tx.send(Done {
                        search: Some(search),
                        ..Done::untagged(AppEvent::SearchDone(search_all(&sources, &query).await))
                    });
                }));
                Vec::new()
            }
            Effect::Suggest(prefix) => {
                if let Some(pending) = self.suggesting.take() {
                    pending.abort();
                }
                let (suggester, tx) = (self.suggester.clone(), self.tx.clone());
                self.suggesting = Some(tokio::spawn(async move {
                    tokio::time::sleep(SUGGEST_DELAY).await;
                    let found = suggester.suggest(&prefix).await;
                    let _ = tx.send(Done::untagged(AppEvent::Suggested(prefix, found)));
                }));
                Vec::new()
            }
            Effect::LoadProgress { keys, title } => self.load_progress(&keys, &title, app),
            Effect::SavePage {
                ch,
                source,
                page,
                page_count,
            } => self.with_series(|db, id| db.save_page(id, ch, &source, page, page_count)),
            Effect::SetRead(ch, read) => {
                let tracking = self.tracking.as_ref();
                self.with_series(|db, id| {
                    db.set_read(id, ch, read)?;
                    match tracking {
                        Some(t) if read => t.read(db, id),
                        _ => Ok(()),
                    }
                })
            }
            Effect::ListPages(source, ch) => {
                self.reading += 1;
                let n = ch.number;
                self.reading_task(source, |src| async move {
                    AppEvent::PagesListed(n, src.pages(&ch).await)
                })
                .map_or_else(|e| vec![AppEvent::PagesListed(n, Err(e))], |()| Vec::new())
            }
            Effect::FetchPage(source, index, page) => self
                .reading_task(source, |src| async move {
                    AppEvent::PageLoaded(index, fetch_image(src, page).await)
                })
                .map_or_else(
                    |e| vec![AppEvent::PageLoaded(index, Err(e))],
                    |()| Vec::new(),
                ),
            Effect::Download {
                source,
                ch,
                then_open,
            } => self.download(source, ch, then_open),
            Effect::SearchLink => self.ask_link(false, app),
            Effect::Link(choice) => self.link(choice),
            Effect::ApplyRemote { id, progress } => self.apply_remote(id, progress, app),
        }
    }

    /// Sends what's queued before hon exits, waiting a moment at most.
    async fn finish_tracking(&self) {
        if let Some(t) = &self.tracking {
            t.syncer.finish(track::FINISH_TIMEOUT).await;
        }
    }

    /// The open series' AniList link, when logged in.
    fn current_link(&self) -> Option<(i64, &Tracking, Option<hon_core::db::Link>)> {
        let t = self.tracking.as_ref()?;
        let (id, _) = self.series.as_ref()?;
        let link = self.db.link(*id, TRACKER, &t.account()).ok()?;
        Some((*id, t, link))
    }

    /// Opens the entry picker and searches AniList for the open series' title.
    fn ask_link(&self, first: bool, app: &App) -> Vec<AppEvent> {
        let (Some((series, t, link)), Some((group, _))) = (self.current_link(), &app.chosen) else {
            return Vec::new();
        };
        let (tracker, title, tx) = (t.tracker.clone(), group.title.clone(), self.tx.clone());
        tokio::spawn(async move {
            let found = track::with_timeout(tracker.search(&title))
                .await
                .map_err(|e| track::note(&e));
            let _ = tx.send(Done {
                series: Some(series),
                ..Done::untagged(AppEvent::LinkMatches(found))
            });
        });
        let current = link.and_then(|l| l.remote_id.and(l.title));
        vec![AppEvent::AskLink { first, current }]
    }

    /// Asks AniList for the linked entry's number, when the open series is linked.
    fn pull(&self) -> Vec<AppEvent> {
        let Some((series, t, Some(link))) = self.current_link() else {
            return Vec::new();
        };
        let Some(id) = link.remote_id else {
            return Vec::new();
        };
        let (tracker, tx) = (t.tracker.clone(), self.tx.clone());
        tokio::spawn(async move {
            let pulled = track::with_timeout(tracker.entry(id))
                .await
                .map(|e| Some(e.progress.unwrap_or(0)))
                .map_err(|e| track::note(&e));
            let _ = tx.send(Done {
                series: Some(series),
                ..Done::untagged(AppEvent::Pulled(id, pulled))
            });
        });
        vec![AppEvent::Pulling]
    }

    fn link(&self, choice: Choice) -> Vec<AppEvent> {
        let (Some(t), Some((series, title))) = (&self.tracking, &self.series) else {
            return Vec::new();
        };
        let mut events = match track::record(&self.db, t, *series, title, &choice) {
            Ok(said) if said.is_empty() => Vec::new(),
            Ok(said) => vec![AppEvent::Error(said)],
            Err(e) => return vec![AppEvent::Error(format!("couldn't save the link: {e:#}"))],
        };
        if matches!(choice, Choice::Link(_)) {
            events.extend(self.pull());
        }
        events
    }

    fn apply_remote(&self, id: i64, remote: u32, app: &App) -> Vec<AppEvent> {
        let (Some(t), Some((series, _)), Some((group, _))) =
            (&self.tracking, &self.series, &app.chosen)
        else {
            return Vec::new();
        };
        let chapters = all_chapters(group);
        match track::take_in(&self.db, t, *series, &chapters, id, remote)
            .and_then(|()| self.db.progress(*series))
        {
            Ok(progress) => vec![AppEvent::Progress(progress)],
            Err(e) => vec![AppEvent::Error(format!(
                "couldn't take in AniList's progress: {e:#}"
            ))],
        }
    }

    fn source(&self, id: SourceId) -> anyhow::Result<Arc<dyn Source>> {
        self.sources
            .iter()
            .find(|s| s.id() == id)
            .cloned()
            .ok_or_else(|| anyhow!("{} isn't registered", id.name()))
    }

    /// Spawns `task` with source `id`, tagged with the chapter being read.
    fn reading_task<F>(
        &self,
        id: SourceId,
        task: impl FnOnce(Arc<dyn Source>) -> F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = AppEvent> + Send + 'static,
    {
        let task = task(self.source(id)?);
        let (tx, reading) = (self.tx.clone(), Some(self.reading));
        tokio::spawn(async move {
            let _ = tx.send(Done {
                reading,
                ..Done::untagged(task.await)
            });
        });
        Ok(())
    }

    fn with_series(&self, op: impl FnOnce(&Db, i64) -> anyhow::Result<()>) -> Vec<AppEvent> {
        let Some((id, _)) = &self.series else {
            return vec![AppEvent::Error("no series open".into())];
        };
        match op(&self.db, *id) {
            Ok(()) => Vec::new(),
            Err(e) => vec![AppEvent::Error(format!("couldn't save progress: {e:#}"))],
        }
    }

    /// Answers with the series' progress, then one `Downloaded` per chapter already on disk.
    fn load_progress(&mut self, keys: &[String], title: &str, app: &App) -> Vec<AppEvent> {
        // Until this one loads no series is open, so progress is never saved to the old one.
        self.series = None;
        self.following = None;
        let loaded = self.db.series(keys, title).and_then(|series| {
            // What was read in OpenComic since; a bad progress file doesn't stop the rest.
            let imported = match (&self.config.opencomic, &app.chosen) {
                (Some(storage), Some((_, source))) => {
                    let dl = &self.config.download_dir;
                    import_opencomic(&self.db, storage, dl, &series, source).err()
                }
                _ => None,
            };
            let progress = self.db.progress(series.id)?;
            Ok((series.id, series.title, progress, imported))
        });
        let (id, title, progress, imported) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => return vec![AppEvent::Error(format!("couldn't load progress: {e:#}"))],
        };
        self.series = Some((id, title.clone()));
        // AniList first, so a `hon read` chapter waits for its answer.
        let mut events = Vec::new();
        if let Some(Err(e)) = self.tracking.as_ref().map(|t| t.read(&self.db, id)) {
            events.push(AppEvent::Error(format!(
                "couldn't queue AniList progress: {e:#}"
            )));
        }
        events.extend(match self.current_link() {
            Some((_, _, None)) => self.ask_link(true, app),
            Some(_) => self.pull(),
            None => Vec::new(),
        });
        events.push(AppEvent::Progress(progress));
        if let Some(e) = imported {
            events.push(AppEvent::Error(format!(
                "couldn't read OpenComic's progress: {e:#}"
            )));
        }
        if let Some((group, _)) = &app.chosen {
            let numbers: BTreeSet<ChapterNo> = group
                .sources
                .iter()
                .flat_map(|s| s.chapters.iter().map(|c| c.number))
                .collect();
            events.extend(numbers.into_iter().filter_map(|n| {
                let path =
                    existing_download(&self.config.download_dir, &title, n, self.config.format)?;
                Some(AppEvent::Downloaded(n, Ok(path)))
            }));
        }
        events
    }

    fn download(&mut self, source: SourceId, ch: ChapterRef, then_open: bool) -> Vec<AppEvent> {
        let n = ch.number;
        let Some((series, title)) = &self.series else {
            return vec![AppEvent::Downloaded(n, Err(anyhow!("no series open")))];
        };
        if then_open && opencomic::is_opencomic(self.config.viewer.as_deref()) {
            self.following = Some(Following {
                series: *series,
                since_ms: jiff::Timestamp::now().as_millisecond(),
                seen: None,
                warning: None,
            });
        }
        let series = Some(*series);
        let (dir, fmt) = (&self.config.download_dir, self.config.format);
        let viewer = self.config.viewer.clone();
        let storage = self.config.opencomic.clone();
        if let Some(path) = existing_download(dir, title, n, fmt) {
            return finished(n, path, then_open, viewer.as_deref(), storage.as_deref());
        }
        let src = match self.source(source) {
            Ok(src) => src,
            Err(e) => return vec![AppEvent::Downloaded(n, Err(e))],
        };
        let path = chapter_path(dir, title, n, fmt);
        let tx = self.tx.clone();
        let runtime = tokio::runtime::Handle::current();
        // A blocking thread: the fetch_pages future isn't `Send` (so it can't go into
        // `tokio::spawn`), and writing a PDF is CPU-bound anyway.
        tokio::task::spawn_blocking(move || {
            let saved = runtime
                .block_on(fetch_pages(src.as_ref(), &ch, |_, _| {}))
                .and_then(|pages| write_chapter(&pages, fmt, &path))
                .map(|()| path);
            let events = match saved {
                Ok(path) => finished(n, path, then_open, viewer.as_deref(), storage.as_deref()),
                Err(e) => vec![AppEvent::Downloaded(n, Err(e))],
            };
            for event in events {
                let _ = tx.send(Done {
                    series,
                    ..Done::untagged(event)
                });
            }
        });
        Vec::new()
    }
}

impl Runtime {
    /// One look at OpenComic's progress while following it: when the file changed, takes
    /// in what was read and works out the chapter open now (ignoring progress from before
    /// it was opened from hon).
    fn follow_viewer(&mut self, app: &App) -> Option<AppEvent> {
        let storage = self.config.opencomic.as_ref()?;
        let (id, title) = self.series.as_ref()?;
        let following = self.following.as_mut().filter(|f| f.series == *id)?;
        let (group, source) = app.chosen.as_ref()?;
        let file = storage.join(opencomic::PROGRESS_FILE);
        let modified = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        if following.seen == Some(modified) {
            return None;
        }
        let series = Series {
            id: *id,
            title: title.clone(),
        };
        let dl = &self.config.download_dir;
        let looked = import_opencomic(&self.db, storage, dl, &series, source)
            .and_then(|entries| Ok((entries, self.db.progress(*id)?)));
        let (entries, progress) = match looked {
            Ok(looked) => looked,
            // OpenComic may be halfway through writing it; try again next time.
            Err(e) => {
                following.seen = None;
                let warning = format!("couldn't read OpenComic's progress: {e:#}");
                if following.warning.as_ref() == Some(&warning) {
                    return None;
                }
                following.warning = Some(warning.clone());
                return Some(AppEvent::Error(warning));
            }
        };
        following.seen = Some(modified);
        following.warning = None;
        if let Some(t) = &self.tracking {
            let _ = t.read(&self.db, *id);
        }
        let recent: Vec<opencomic::Entry> = entries
            .into_iter()
            .filter(|e| e.last_reading_ms >= following.since_ms)
            .collect();
        let reading = opencomic::current_chapter(&recent, &all_chapters(group));
        Some(AppEvent::ViewerProgress { progress, reading })
    }
}

/// Fetches a page and decodes it on a blocking thread.
async fn fetch_image(src: Arc<dyn Source>, page: PageRef) -> anyhow::Result<Arc<DynamicImage>> {
    let bytes = src.fetch_page(&page).await?;
    tokio::task::spawn_blocking(move || image::load_from_memory(&bytes))
        .await?
        .map(Arc::new)
        .context("not an image")
}

/// `Downloaded`, then `Opened` when asked to open the file in the viewer.
fn finished(
    n: ChapterNo,
    path: PathBuf,
    then_open: bool,
    viewer: Option<&str>,
    opencomic: Option<&Path>,
) -> Vec<AppEvent> {
    let opened = then_open.then(|| open_chapter(&path, viewer, opencomic));
    let mut events = vec![AppEvent::Downloaded(n, Ok(path))];
    events.extend(opened.map(|result| AppEvent::Opened(n, result)));
    events
}

#[cfg(test)]
mod tests {
    use hon_core::model::test_ids::*;
    use std::time::Duration;

    use crossterm::event::KeyCode;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::config::{Paths, Reader};
    use crate::tui::app::test_data::{key, n, searched};
    use hon_core::download::Format;
    use hon_core::download::test_images;
    use hon_core::model::MangaRef;

    /// Finds one title named after the query, with chapters 1–3 of one page each.
    struct Fake;

    #[async_trait::async_trait]
    impl Source for Fake {
        fn id(&self) -> SourceId {
            alpha()
        }
        async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>> {
            Ok(vec![MangaRef {
                source: alpha(),
                id: query.into(),
                title: query.into(),
                alt_titles: Vec::new(),
                url: String::new(),
            }])
        }
        async fn chapters(&self, _: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
            Ok((1..=3).map(chapter).collect())
        }
        async fn pages(&self, _: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
            Ok(vec![PageRef { url: "p".into() }])
        }
        async fn fetch_page(&self, page: &PageRef) -> anyhow::Result<Vec<u8>> {
            match page.url.as_str() {
                "garbage" => Ok(b"<html>rate limited</html>".to_vec()),
                _ => Ok(test_images::jpeg_rgb()),
            }
        }
    }

    fn chapter(v: u32) -> ChapterRef {
        ChapterRef {
            source: alpha(),
            id: v.to_string(),
            number: n(v),
            title: None,
            published: None,
        }
    }

    fn fake_runtime() -> (tempfile::TempDir, Runtime, UnboundedReceiver<Done>) {
        runtime_with(vec![Arc::new(Fake)])
    }

    fn runtime_with(
        sources: Vec<Arc<dyn Source>>,
    ) -> (tempfile::TempDir, Runtime, UnboundedReceiver<Done>) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            sources,
            db: Db::open_in_memory().unwrap(),
            config: Config::load(&Paths::under(dir.path())).unwrap(),
            tracking: None,
        };
        let (tx, rx) = unbounded_channel();
        (dir, Runtime::new(ctx, tx), rx)
    }

    async fn next(rx: &mut UnboundedReceiver<Done>) -> Done {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("effect answered")
            .unwrap()
    }

    #[tokio::test]
    async fn superseded_search_results_are_dropped() {
        let (_dir, mut runtime, mut rx) = fake_runtime();
        let app = App::new(Start::Search, Reader::Terminal).0;
        runtime.execute(Effect::Search("first".into()), &app);
        runtime.execute(Effect::Search("second".into()), &app);
        let Some(AppEvent::SearchDone(out)) = runtime.current(next(&mut rx).await) else {
            panic!("expected the second search's results");
        };
        assert_eq!(out.groups[0].title, "second");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "the first search was cancelled");
    }

    /// Searches that never finish, counting the ones dropped unfinished.
    struct Stuck(Arc<std::sync::atomic::AtomicUsize>);

    struct CountDrop(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl Source for Stuck {
        fn id(&self) -> SourceId {
            alpha()
        }
        async fn search(&self, _: &str) -> anyhow::Result<Vec<MangaRef>> {
            let _counted = CountDrop(self.0.clone());
            std::future::pending().await
        }
        async fn chapters(&self, _: &MangaRef) -> anyhow::Result<Vec<ChapterRef>> {
            unreachable!()
        }
        async fn pages(&self, _: &ChapterRef) -> anyhow::Result<Vec<PageRef>> {
            unreachable!()
        }
        async fn fetch_page(&self, _: &PageRef) -> anyhow::Result<Vec<u8>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn new_search_cancels_the_running_one() {
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_dir, mut runtime, _rx) = runtime_with(vec![Arc::new(Stuck(dropped.clone()))]);
        let app = App::new(Start::Search, Reader::Terminal).0;
        let count = || dropped.load(std::sync::atomic::Ordering::SeqCst);
        runtime.execute(Effect::Search("first".into()), &app);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(count(), 0, "the first search is running");
        runtime.execute(Effect::Search("second".into()), &app);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(count(), 1, "the first search's calls were dropped");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_for_a_closed_series_is_dropped() {
        let (dir, mut runtime, mut rx) = fake_runtime();
        let app = App::new(Start::Search, Reader::Terminal).0;
        let load = |title: &str| Effect::LoadProgress {
            keys: vec![title.to_lowercase()],
            title: title.into(),
        };
        let download = |v| Effect::Download {
            source: alpha(),
            ch: chapter(v),
            then_open: false,
        };

        runtime.execute(load("A"), &app);
        runtime.execute(download(1), &app);
        let done = runtime.current(next(&mut rx).await);
        assert!(
            matches!(done, Some(AppEvent::Downloaded(_, Ok(_)))),
            "{done:?}"
        );

        runtime.execute(download(2), &app);
        runtime.execute(load("B"), &app);
        let done = next(&mut rx).await;
        assert!(matches!(done.event, AppEvent::Downloaded(_, Ok(_))));
        assert!(runtime.current(done).is_none(), "B is open now");
        let written = chapter_path(&dir.path().join("downloads"), "A", n(2), Format::Pdf);
        assert!(written.exists(), "the file is still written");

        runtime.execute(load("A"), &app);
        runtime.execute(download(3), &app);
        assert!(runtime.current(next(&mut rx).await).is_some());
    }

    fn page(url: &str) -> PageRef {
        PageRef { url: url.into() }
    }

    #[tokio::test]
    async fn pages_are_listed_then_fetched_and_decoded() {
        let (_dir, mut runtime, mut rx) = fake_runtime();
        let app = App::new(Start::Search, Reader::Terminal).0;
        runtime.execute(Effect::ListPages(alpha(), chapter(1)), &app);
        match runtime.current(next(&mut rx).await) {
            Some(AppEvent::PagesListed(ch, Ok(pages))) => {
                assert_eq!((ch, pages), (n(1), vec![page("p")]));
            }
            other => panic!("{other:?}"),
        }

        runtime.execute(Effect::FetchPage(alpha(), 0, page("p")), &app);
        match runtime.current(next(&mut rx).await) {
            Some(AppEvent::PageLoaded(0, Ok(image))) => {
                assert_eq!((image.width(), image.height()), (40, 60));
            }
            other => panic!("{other:?}"),
        }

        runtime.execute(Effect::FetchPage(alpha(), 1, page("garbage")), &app);
        match runtime.current(next(&mut rx).await) {
            Some(AppEvent::PageLoaded(1, Err(e))) => {
                assert!(format!("{e:#}").starts_with("not an image"), "{e:#}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn unregistered_source_fails_the_page_not_the_app() {
        let (_dir, mut runtime, _rx) = fake_runtime();
        let app = App::new(Start::Search, Reader::Terminal).0;
        let listed = runtime.execute(Effect::ListPages(beta(), chapter(1)), &app);
        assert!(
            matches!(&listed[..], [AppEvent::PagesListed(_, Err(e))] if e.to_string() == "Beta isn't registered"),
            "{listed:?}"
        );
        let fetched = runtime.execute(Effect::FetchPage(beta(), 2, page("p")), &app);
        assert!(
            matches!(&fetched[..], [AppEvent::PageLoaded(2, Err(_))]),
            "{fetched:?}"
        );
    }

    #[tokio::test]
    async fn pages_of_a_previous_chapter_are_dropped() {
        let (_dir, mut runtime, mut rx) = fake_runtime();
        let app = App::new(Start::Search, Reader::Terminal).0;
        runtime.execute(Effect::ListPages(alpha(), chapter(1)), &app);
        runtime.execute(Effect::FetchPage(alpha(), 0, page("p")), &app);
        runtime.execute(Effect::ListPages(alpha(), chapter(2)), &app);
        let mut shown = Vec::new();
        for _ in 0..3 {
            if let Some(ev) = runtime.current(next(&mut rx).await) {
                shown.push(format!("{ev:?}"));
            }
        }
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(
            shown[0].starts_with("PagesListed(ChapterNo(2.0)"),
            "{shown:?}"
        );
    }

    #[test]
    fn save_page_runs_inline() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            sources: Vec::new(),
            db: Db::open_in_memory().unwrap(),
            config: Config::load(&Paths::under(dir.path())).unwrap(),
            tracking: None,
        };
        let mut runtime = Runtime::new(ctx, tx);
        let app = App::new(Start::Search, Reader::Terminal).0;
        let save = Effect::SavePage {
            ch: n(112),
            source: beta(),
            page: 13,
            page_count: 38,
        };
        assert!(matches!(
            &runtime.execute(save.clone(), &app)[..],
            [AppEvent::Error(_)]
        ));

        let load = Effect::LoadProgress {
            keys: vec!["solo leveling".into()],
            title: "Solo Leveling".into(),
        };
        runtime.execute(load, &app);
        assert!(runtime.execute(save, &app).is_empty());
        let id = runtime
            .db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap()
            .id;
        let p = &runtime.db.progress(id).unwrap()[&n(112)];
        assert_eq!(
            (p.last_page, p.page_count, p.source.as_deref()),
            (Some(13), Some(38), Some("beta"))
        );
    }

    #[test]
    fn loading_a_series_takes_in_opencomic_progress() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::write(&paths.config_file, "viewer = \"opencomic\"\n").unwrap();
        std::fs::create_dir_all(&paths.opencomic_storage[0]).unwrap();
        let config = Config::load(&paths).unwrap();
        let storage = config.opencomic.clone().expect("found the storage folder");
        let file = chapter_path(&config.download_dir, "Solo Leveling", n(112), Format::Cbz);
        let later = (hon_core::db::now() + 60) * 1000;
        // Built with serde_json so a Windows path's backslashes are escaped.
        let progress = serde_json::json!({
            file.display().to_string():
                {"page": 9, "pages": 38, "completed": false, "lastReading": later}
        });
        std::fs::write(storage.join("readingProgress.json"), progress.to_string()).unwrap();
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            sources: Vec::new(),
            db: Db::open_in_memory().unwrap(),
            config,
            tracking: None,
        };
        let mut runtime = Runtime::new(ctx, tx);
        let mut app = searched();
        app.update(key(KeyCode::Enter));

        let load = Effect::LoadProgress {
            keys: vec!["solo leveling".into()],
            title: "Solo Leveling".into(),
        };
        let events = runtime.execute(load, &app);
        assert!(
            !events.iter().any(|e| matches!(e, AppEvent::Error(_))),
            "{events:?}"
        );
        let Some(AppEvent::Progress(loaded)) = events.first() else {
            panic!("{events:?}");
        };
        let p = &loaded[&n(112)];
        assert_eq!(
            (p.read, p.last_page, p.page_count, p.source.as_deref()),
            (false, Some(8), Some(38), Some("beta"))
        );
    }

    #[test]
    fn opening_in_opencomic_follows_its_progress() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::write(&paths.config_file, "viewer = \"opencomic\"\n").unwrap();
        std::fs::create_dir_all(&paths.opencomic_storage[0]).unwrap();
        let config = Config::load(&paths).unwrap();
        let storage = config.opencomic.clone().expect("found the storage folder");
        let file = chapter_path(&config.download_dir, "Solo Leveling", n(112), Format::Cbz);
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            sources: Vec::new(),
            db: Db::open_in_memory().unwrap(),
            config,
            tracking: None,
        };
        let mut runtime = Runtime::new(ctx, tx);
        let mut app = searched();
        app.update(key(KeyCode::Enter));
        let load = Effect::LoadProgress {
            keys: vec!["solo leveling".into()],
            title: "Solo Leveling".into(),
        };
        runtime.execute(load, &app);
        assert!(runtime.follow_viewer(&app).is_none(), "nothing opened yet");

        let ch = app.chapters().iter().find(|c| c.number == n(112)).unwrap();
        let open = Effect::Download {
            source: app.chosen.as_ref().unwrap().1.clone(),
            ch: ch.clone(),
            then_open: true,
        };
        runtime.execute(open, &app);
        let later = (hon_core::db::now() + 60) * 1000;
        let progress = serde_json::json!({
            file.display().to_string():
                {"page": 38, "pages": 38, "completed": true, "lastReading": later}
        });
        std::fs::write(storage.join("readingProgress.json"), progress.to_string()).unwrap();

        let Some(AppEvent::ViewerProgress { progress, reading }) = runtime.follow_viewer(&app)
        else {
            panic!("expected OpenComic's progress");
        };
        assert_eq!(reading, Some(n(113)), "finished 112, so on to 113");
        assert!(progress[&n(112)].read);
        assert!(
            runtime.follow_viewer(&app).is_none(),
            "the file hasn't changed since"
        );

        runtime.execute(
            Effect::LoadProgress {
                keys: vec!["solo leveling".into()],
                title: "Solo Leveling".into(),
            },
            &app,
        );
        std::fs::write(storage.join("readingProgress.json"), "{}").unwrap();
        assert!(
            runtime.follow_viewer(&app).is_none(),
            "loading a series stops following"
        );
    }

    #[test]
    fn downloads_are_found_under_the_series_first_title() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&Paths::under(dir.path())).unwrap();
        let db = Db::open_in_memory().unwrap();
        let keys = vec!["na honjaman level up".into(), "solo leveling".into()];
        db.series(&keys, "Na Honjaman Level-Up").unwrap();
        let on_disk = chapter_path(
            &config.download_dir,
            "Na Honjaman Level-Up",
            n(1),
            Format::Pdf,
        );
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, b"pdf").unwrap();
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            sources: vec![Arc::new(Fake)],
            db,
            config,
            tracking: None,
        };
        let mut runtime = Runtime::new(ctx, tx);
        let (app, effects) = App::new(
            Start::Read {
                group: TitleGroup {
                    keys: vec!["solo leveling".into()],
                    title: "Solo Leveling".into(),
                    sources: searched().groups[0].sources.clone(),
                    unavailable: Vec::new(),
                },
                source: beta(),
                chapter: None,
            },
            Reader::Terminal,
        );
        let events: Vec<AppEvent> = effects
            .into_iter()
            .flat_map(|e| runtime.execute(e, &app))
            .collect();
        assert!(
            matches!(&events[..], [AppEvent::Progress(_), AppEvent::Downloaded(ch, Ok(path))] if *ch == n(1) && *path == on_disk),
            "{events:?}"
        );
    }

    #[test]
    fn failed_load_closes_the_previous_series() {
        let (_dir, mut runtime, _rx) = fake_runtime();
        let app = App::new(Start::Search, Reader::Terminal).0;
        let load = |keys: &[&str]| Effect::LoadProgress {
            keys: keys.iter().map(|k| k.to_string()).collect(),
            title: "T".into(),
        };
        runtime.execute(load(&["a"]), &app);
        let failed = runtime.execute(load(&[]), &app);
        assert!(
            matches!(&failed[..], [AppEvent::Error(e)] if e.starts_with("couldn't load progress")),
            "{failed:?}"
        );
        let save = Effect::SavePage {
            ch: n(1),
            source: alpha(),
            page: 3,
            page_count: 9,
        };
        assert!(
            matches!(&runtime.execute(save, &app)[..], [AppEvent::Error(e)] if e == "no series open")
        );
        assert!(matches!(
            &runtime.execute(Effect::SetRead(n(1), true), &app)[..],
            [AppEvent::Error(_)]
        ));
        let a = runtime.db.series(&["a".into()], "A").unwrap();
        assert!(runtime.db.progress(a.id).unwrap().is_empty(), "A untouched");
    }

    #[test]
    fn db_effects_run_inline() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&Paths::under(dir.path())).unwrap();
        let db = Db::open_in_memory().unwrap();
        let id = db
            .series(&["solo leveling".into()], "Solo Leveling")
            .unwrap()
            .id;
        db.set_read(id, n(3), true).unwrap();
        let on_disk = chapter_path(&config.download_dir, "Solo Leveling", n(114), Format::Cbz);
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, b"cbz").unwrap();
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            sources: Vec::new(),
            db,
            config,
            tracking: None,
        };
        let mut runtime = Runtime::new(ctx, tx);

        let mut app = searched();
        let effects = app.update(key(KeyCode::Enter));
        let events: Vec<AppEvent> = effects
            .into_iter()
            .flat_map(|e| runtime.execute(e, &app))
            .collect();
        match &events[..] {
            [AppEvent::Progress(p), AppEvent::Downloaded(ch, Ok(path))] => {
                assert!(p[&n(3)].read);
                assert_eq!(*ch, n(114));
                assert_eq!(path, &on_disk);
            }
            other => panic!("{other:?}"),
        }

        assert!(
            runtime
                .execute(Effect::SetRead(n(5), true), &app)
                .is_empty()
        );
        assert!(runtime.db.progress(id).unwrap()[&n(5)].read);

        let ch = app.chapters()[113].clone();
        let download = Effect::Download {
            source: beta(),
            ch,
            then_open: false,
        };
        match &runtime.execute(download, &app)[..] {
            [AppEvent::Downloaded(ch, Ok(path))] => {
                assert_eq!((*ch, path), (n(114), &on_disk), "reuses the file on disk");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn logged_in_open_asks_first_and_reading_is_sent() {
        use crate::commands::track::Choice;
        use hon_core::track::fake::Fake as FakeTracker;
        use hon_core::track::login::Login;
        use hon_core::track::{Match, Status};

        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("hon.db");
        let m = Match {
            id: 105398,
            title: "Solo Leveling".into(),
            format: None,
            year: None,
            chapters: None,
        };
        let tracker = Arc::new(FakeTracker {
            matches: vec![m.clone()],
            ..FakeTracker::default()
        });
        let login = Login {
            token: "t".into(),
            user_id: 8,
            name: "sem".into(),
        };
        let ctx = Ctx {
            sources: vec![Arc::new(Fake)],
            db: Db::open(&db_file).unwrap(),
            config: Config::load(&Paths::under(dir.path())).unwrap(),
            tracking: Some(Tracking::with(login, tracker.clone(), &db_file)),
        };
        let (tx, mut rx) = unbounded_channel();
        let mut runtime = Runtime::new(ctx, tx);
        let mut app = searched();
        let open = app.update(key(KeyCode::Enter));
        let events: Vec<AppEvent> = open
            .into_iter()
            .flat_map(|e| runtime.execute(e, &app))
            .collect();
        assert!(
            matches!(
                &events[..],
                [
                    AppEvent::AskLink {
                        first: true,
                        current: None
                    },
                    AppEvent::Progress(_),
                    ..
                ]
            ),
            "{events:?}"
        );
        let done = rx.recv().await.unwrap();
        assert!(
            matches!(&done.event, AppEvent::LinkMatches(Ok(found)) if *found == [m.clone()]),
            "{:?}",
            done.event
        );

        let events = runtime.execute(Effect::Link(Choice::Link(m)), &app);
        assert!(
            matches!(&events[..], [AppEvent::Error(said), AppEvent::Pulling] if said.starts_with("tracking")),
            "{events:?}"
        );
        let done = rx.recv().await.unwrap();
        assert!(matches!(done.event, AppEvent::Pulled(105398, Ok(Some(0)))));

        assert!(
            runtime
                .execute(Effect::SetRead(n(2), true), &app)
                .is_empty()
        );
        runtime.finish_tracking().await;
        assert_eq!(
            *tracker.saves.lock().unwrap(),
            [(105398, 2, Status::Current)]
        );
    }
}
