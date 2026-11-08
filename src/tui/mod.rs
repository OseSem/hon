pub mod app;
pub mod ui;

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use image::DynamicImage;
use ratatui::DefaultTerminal;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::commands::{Ctx, import_opencomic};
use crate::config::Config;
use crate::db::Db;
use crate::download::{chapter_path, existing_download, fetch_pages, write_chapter};
use crate::model::{ChapterNo, ChapterRef, PageRef, SourceId};
use crate::search::{TitleGroup, search_all};
use crate::sources::Source;
use crate::viewer::{open_in_viewer, open_target, opencomic};
use app::{App, AppEvent, Effect};
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
    let (encoded_tx, mut encoded) = unbounded_channel();
    let mut view = PageView::new(picker, encoded_tx);
    let mut terminal_events = EventStream::new();
    let (mut app, mut effects) = App::new(start, reader_mode);
    app.viewer_reports_progress = viewer_reports_progress;
    loop {
        // DB effects run right here and answer with events, which may ask for more effects.
        while !effects.is_empty() {
            let mut events = Vec::new();
            for effect in std::mem::take(&mut effects) {
                if effect == Effect::Quit {
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
            ev = terminal_events.next() => match ev {
                Some(Ok(Event::Key(key))) => AppEvent::Key(key),
                Some(Ok(Event::Resize(w, h))) => AppEvent::Resize(w, h),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
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
    /// Bumped by every `ListPages`, so pages only land in the chapter they were fetched for.
    reading: u64,
}

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
            reading: 0,
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
                let (sources, tx, search) = (self.sources.clone(), self.tx.clone(), self.search);
                tokio::spawn(async move {
                    let _ = tx.send(Done {
                        search: Some(search),
                        ..Done::untagged(AppEvent::SearchDone(search_all(&sources, &query).await))
                    });
                });
                Vec::new()
            }
            Effect::LoadProgress { keys, title } => self.load_progress(&keys, &title, app),
            Effect::SavePage {
                ch,
                source,
                page,
                page_count,
            } => self.with_series(|db, id| db.save_page(id, ch, source, page, page_count)),
            Effect::SetRead(ch, read) => self.with_series(|db, id| db.set_read(id, ch, read)),
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
        let loaded = self.db.series(keys, title).and_then(|series| {
            // What was read in OpenComic since; a bad progress file doesn't stop the rest.
            let imported = match (&self.config.opencomic, &app.chosen) {
                (Some(storage), Some((_, source))) => {
                    let dl = &self.config.download_dir;
                    import_opencomic(&self.db, storage, dl, &series, *source).err()
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
        let mut events = vec![AppEvent::Progress(progress)];
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
        self.series = Some((id, title));
        events
    }

    fn download(&self, source: SourceId, ch: ChapterRef, then_open: bool) -> Vec<AppEvent> {
        let n = ch.number;
        let Some((series, title)) = &self.series else {
            return vec![AppEvent::Downloaded(n, Err(anyhow!("no series open")))];
        };
        let series = Some(*series);
        let (dir, fmt) = (&self.config.download_dir, self.config.format);
        let viewer = self.config.viewer.clone();
        if let Some(path) = existing_download(dir, title, n, fmt) {
            return finished(n, path, then_open, viewer.as_deref());
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
                Ok(path) => finished(n, path, then_open, viewer.as_deref()),
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

/// Fetches a page and decodes it on a blocking thread.
async fn fetch_image(src: Arc<dyn Source>, page: PageRef) -> anyhow::Result<Arc<DynamicImage>> {
    let bytes = src.fetch_page(&page).await?;
    tokio::task::spawn_blocking(move || image::load_from_memory(&bytes))
        .await?
        .map(Arc::new)
        .context("not an image")
}

/// `Downloaded`, then `Opened` when asked to open the file in the viewer.
fn finished(n: ChapterNo, path: PathBuf, then_open: bool, viewer: Option<&str>) -> Vec<AppEvent> {
    let opened = then_open.then(|| open_in_viewer(open_target(&path, viewer), viewer));
    let mut events = vec![AppEvent::Downloaded(n, Ok(path))];
    events.extend(opened.map(|result| AppEvent::Opened(n, result)));
    events
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::KeyCode;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::config::{Format, Paths, Reader};
    use crate::download::test_images;
    use crate::model::MangaRef;
    use crate::tui::app::test_data::{key, n, searched};

    /// Finds one title named after the query, with chapters 1–3 of one page each.
    struct Fake;

    #[async_trait::async_trait]
    impl Source for Fake {
        fn id(&self) -> SourceId {
            SourceId::MangaDex
        }
        async fn search(&self, query: &str) -> anyhow::Result<Vec<MangaRef>> {
            Ok(vec![MangaRef {
                source: SourceId::MangaDex,
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
            source: SourceId::MangaDex,
            id: v.to_string(),
            number: n(v),
            title: None,
            published: None,
        }
    }

    fn fake_runtime() -> (tempfile::TempDir, Runtime, UnboundedReceiver<Done>) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            sources: vec![Arc::new(Fake)],
            db: Db::open_in_memory().unwrap(),
            config: Config::load(&Paths::under(dir.path())).unwrap(),
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
        let mut shown = Vec::new();
        for _ in 0..2 {
            if let Some(AppEvent::SearchDone(out)) = runtime.current(next(&mut rx).await) {
                shown.push(out.groups[0].title.clone());
            }
        }
        assert_eq!(shown, ["second"]);
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
            source: SourceId::MangaDex,
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
        runtime.execute(Effect::ListPages(SourceId::MangaDex, chapter(1)), &app);
        match runtime.current(next(&mut rx).await) {
            Some(AppEvent::PagesListed(ch, Ok(pages))) => {
                assert_eq!((ch, pages), (n(1), vec![page("p")]));
            }
            other => panic!("{other:?}"),
        }

        runtime.execute(Effect::FetchPage(SourceId::MangaDex, 0, page("p")), &app);
        match runtime.current(next(&mut rx).await) {
            Some(AppEvent::PageLoaded(0, Ok(image))) => {
                assert_eq!((image.width(), image.height()), (40, 60));
            }
            other => panic!("{other:?}"),
        }

        runtime.execute(
            Effect::FetchPage(SourceId::MangaDex, 1, page("garbage")),
            &app,
        );
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
        let listed = runtime.execute(Effect::ListPages(SourceId::MangaKatana, chapter(1)), &app);
        assert!(
            matches!(&listed[..], [AppEvent::PagesListed(_, Err(e))] if e.to_string() == "MangaKatana isn't registered"),
            "{listed:?}"
        );
        let fetched = runtime.execute(Effect::FetchPage(SourceId::MangaKatana, 2, page("p")), &app);
        assert!(
            matches!(&fetched[..], [AppEvent::PageLoaded(2, Err(_))]),
            "{fetched:?}"
        );
    }

    #[tokio::test]
    async fn pages_of_a_previous_chapter_are_dropped() {
        let (_dir, mut runtime, mut rx) = fake_runtime();
        let app = App::new(Start::Search, Reader::Terminal).0;
        runtime.execute(Effect::ListPages(SourceId::MangaDex, chapter(1)), &app);
        runtime.execute(Effect::FetchPage(SourceId::MangaDex, 0, page("p")), &app);
        runtime.execute(Effect::ListPages(SourceId::MangaDex, chapter(2)), &app);
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
        };
        let mut runtime = Runtime::new(ctx, tx);
        let app = App::new(Start::Search, Reader::Terminal).0;
        let save = Effect::SavePage {
            ch: n(112),
            source: SourceId::MangaKatana,
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
            (p.last_page, p.page_count, p.source),
            (Some(13), Some(38), Some(SourceId::MangaKatana))
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
        let later = (crate::db::now() + 60) * 1000;
        let progress = format!(
            r#"{{"{}": {{"page": 9, "pages": 38, "completed": false, "lastReading": {later}}}}}"#,
            file.display()
        );
        std::fs::write(storage.join("readingProgress.json"), progress).unwrap();
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            sources: Vec::new(),
            db: Db::open_in_memory().unwrap(),
            config,
        };
        let mut runtime = Runtime::new(ctx, tx);
        let mut app = searched();
        app.update(key(KeyCode::Enter));

        let load = Effect::LoadProgress {
            keys: vec!["solo leveling".into()],
            title: "Solo Leveling".into(),
        };
        let events = runtime.execute(load, &app);
        let Some(AppEvent::Progress(loaded)) = events.first() else {
            panic!("{events:?}");
        };
        let p = &loaded[&n(112)];
        assert_eq!(
            (p.read, p.last_page, p.page_count, p.source),
            (false, Some(8), Some(38), Some(SourceId::MangaKatana))
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
        };
        let mut runtime = Runtime::new(ctx, tx);
        let (app, effects) = App::new(
            Start::Read {
                group: TitleGroup {
                    keys: vec!["solo leveling".into()],
                    title: "Solo Leveling".into(),
                    sources: searched().groups[0].sources.clone(),
                },
                source: SourceId::MangaKatana,
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
            source: SourceId::MangaDex,
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
            source: SourceId::MangaKatana,
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
}
