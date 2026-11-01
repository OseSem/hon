pub mod app;
pub mod ui;

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::commands::Ctx;
use crate::config::{Config, Format};
use crate::db::Db;
use crate::download::{chapter_path, fetch_pages, write_chapter};
use crate::model::{ChapterNo, ChapterRef, SourceId};
use crate::search::{TitleGroup, search_all};
use crate::sources::Source;
use crate::viewer::open_in_viewer;
use app::{App, AppEvent, Effect};

pub enum Start {
    Search,
    Read {
        group: TitleGroup,
        source: SourceId,
        chapter: Option<ChapterNo>,
    },
}

pub async fn run(ctx: Ctx, start: Start) -> anyhow::Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("hon needs an interactive terminal (try `hon search`)");
    }
    // Enters raw mode and the alternate screen, and installs a panic hook that
    // restores the terminal before the panic message is printed.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, ctx, start).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, ctx: Ctx, start: Start) -> anyhow::Result<()> {
    let (tx, mut results) = unbounded_channel();
    let mut runtime = Runtime {
        sources: ctx.sources,
        db: ctx.db,
        config: ctx.config,
        tx,
        series: None,
    };
    let mut terminal_events = EventStream::new();
    let (mut app, mut effects) = App::new(start);
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
        terminal.draw(|f| ui::draw(f, &mut app))?;
        let ev = tokio::select! {
            Some(ev) = results.recv() => ev,
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
    tx: UnboundedSender<AppEvent>,
    /// Series id and canonical title of the open title group.
    series: Option<(i64, String)>,
}

impl Runtime {
    fn execute(&mut self, effect: Effect, app: &App) -> Vec<AppEvent> {
        match effect {
            Effect::Quit => Vec::new(),
            Effect::Search(query) => {
                let sources = self.sources.clone();
                self.spawn(async move { AppEvent::SearchDone(search_all(&sources, &query).await) });
                Vec::new()
            }
            Effect::LoadProgress { key, title } => self.load_progress(&key, title, app),
            Effect::SavePage {
                ch,
                source,
                page,
                page_count,
            } => self.with_series(|db, id| db.save_page(id, ch, source, page, page_count)),
            Effect::SetRead(ch, read) => self.with_series(|db, id| db.set_read(id, ch, read)),
            Effect::ListPages(source, ch) => self.with_source(source, |src| async move {
                AppEvent::PagesListed(ch.number, src.pages(&ch).await)
            }),
            Effect::FetchPage(source, index, page) => self.with_source(source, |src| async move {
                AppEvent::PageLoaded(index, src.fetch_page(&page).await)
            }),
            Effect::Download {
                source,
                ch,
                then_open,
            } => self.download(source, ch, then_open),
        }
    }

    fn spawn(&self, task: impl Future<Output = AppEvent> + Send + 'static) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(task.await);
        });
    }

    fn source(&self, id: SourceId) -> anyhow::Result<Arc<dyn Source>> {
        self.sources
            .iter()
            .find(|s| s.id() == id)
            .cloned()
            .ok_or_else(|| anyhow!("{} isn't registered", id.name()))
    }

    fn with_source<F>(&self, id: SourceId, task: impl FnOnce(Arc<dyn Source>) -> F) -> Vec<AppEvent>
    where
        F: Future<Output = AppEvent> + Send + 'static,
    {
        match self.source(id) {
            Ok(src) => {
                self.spawn(task(src));
                Vec::new()
            }
            Err(e) => vec![AppEvent::Error(format!("{e:#}"))],
        }
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
    fn load_progress(&mut self, key: &str, title: String, app: &App) -> Vec<AppEvent> {
        let loaded = self
            .db
            .series_id(key, &title)
            .and_then(|id| Ok((id, self.db.progress(id)?)));
        let (id, progress) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => return vec![AppEvent::Error(format!("couldn't load progress: {e:#}"))],
        };
        let mut events = vec![AppEvent::Progress(progress)];
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
        let Some((_, title)) = &self.series else {
            return vec![AppEvent::Downloaded(n, Err(anyhow!("no series open")))];
        };
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
            for ev in events {
                let _ = tx.send(ev);
            }
        });
        Vec::new()
    }
}

/// The chapter's file on disk in any format, preferring `preferred`.
fn existing_download(dir: &Path, title: &str, n: ChapterNo, preferred: Format) -> Option<PathBuf> {
    [preferred, Format::Pdf, Format::Cbz, Format::Images]
        .into_iter()
        .map(|fmt| chapter_path(dir, title, n, fmt))
        .find(|path| path.exists())
}

/// `Downloaded`, after opening the file in the viewer when asked to.
fn finished(n: ChapterNo, path: PathBuf, then_open: bool, viewer: Option<&str>) -> Vec<AppEvent> {
    let opened = if then_open {
        open_in_viewer(&path, viewer)
    } else {
        Ok(())
    };
    let mut events = vec![AppEvent::Downloaded(n, Ok(path))];
    if let Err(e) = opened {
        events.push(AppEvent::Error(format!("couldn't open Ch {n}: {e:#}")));
    }
    events
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::config::Paths;
    use crate::tui::app::test_data::{key, n, searched};

    #[test]
    fn db_effects_run_inline() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&Paths::under(dir.path())).unwrap();
        let db = Db::open_in_memory().unwrap();
        let id = db.series_id("solo leveling", "Solo Leveling").unwrap();
        db.set_read(id, n(3), true).unwrap();
        let on_disk = chapter_path(&config.download_dir, "Solo Leveling", n(114), Format::Cbz);
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, b"cbz").unwrap();
        let (tx, _rx) = unbounded_channel();
        let mut runtime = Runtime {
            sources: Vec::new(),
            db,
            config,
            tx,
            series: None,
        };

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
