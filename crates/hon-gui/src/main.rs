//! hon's desktop reader: your library, search across every installed source, and a reader
//! that shows chapters as a long strip or page by page. Progress goes to the same database
//! as the terminal app, so either one picks up where the other left off.

mod covers;
mod reader;
mod settings;
mod strip;
mod theme;
mod views;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use hon::commands::track::Tracking;
use hon::commands::{Ctx, NO_SOURCES};
use hon::config::{Config, Paths};
use hon_core::db::{Db, LibraryEntry, Progress, Series, resume_chapter, resume_page};
use hon_core::model::{ChapterNo, ChapterRef, SourceId, normalize_title};
use hon_core::plugins::store::Store;
use hon_core::search::{SearchOutcome, TitleGroup, search_all};
use iced::keyboard::{self, Key, key::Named};
use iced::{Element, Size, Subscription, Task, Theme, window};
use tokio::runtime::Handle;

use crate::covers::{Cover, Covers, Trend};
use crate::reader::{Opening, Reader};
use crate::settings::{Backdrop, Fit, Mode, Prefs, Sort, View};

const WINDOW: Size = Size::new(1180.0, 900.0);
const SEARCH_INPUT: &str = "search";

fn main() -> iced::Result {
    abort_on_panic();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let started = {
        let _inside = rt.enter();
        start().map_err(|e| format!("{e:#}"))
    };
    let ctx = started.as_ref().ok().map(|s| s.ctx.clone());
    let boot = Cell::new(Some((started, Launch::parse(std::env::args().skip(1)))));
    let handle = rt.handle().clone();
    let result = iced::application(
        move || {
            let (started, launch) = boot.take().expect("booted once");
            App::boot(started, launch, handle.clone())
        },
        App::update,
        App::view,
    )
    .title("hon")
    .window(window_settings())
    .default_font(theme::BODY)
    .theme(App::theme)
    .subscription(App::subscription);
    let result = theme::FONTS
        .into_iter()
        .fold(result, |app, font| app.font(font))
        .run();
    if let Some(ctx) = ctx {
        rt.block_on(ctx.finish_tracking());
    }
    result
}

/// The window: content runs under a hidden title bar on macOS, as media apps do.
fn window_settings() -> window::Settings {
    window::Settings {
        size: WINDOW,
        min_size: Some(Size::new(940.0, 620.0)),
        #[cfg(target_os = "macos")]
        platform_specific: window::settings::PlatformSpecific {
            title_hidden: true,
            titlebar_transparent: true,
            fullsize_content_view: true,
        },
        ..window::Settings::default()
    }
}

/// `hon-gui [TITLE] [--read]`: search for TITLE on launch; with `--read`, open its first
/// result where reading left off.
#[derive(Debug, Default, PartialEq)]
struct Launch {
    query: String,
    read: bool,
}

impl Launch {
    fn parse(args: impl Iterator<Item = String>) -> Launch {
        let mut launch = Launch::default();
        let mut words = Vec::new();
        for arg in args {
            match arg.as_str() {
                "--read" => launch.read = true,
                _ => words.push(arg),
            }
        }
        launch.query = words.join(" ");
        launch
    }
}

struct Started {
    ctx: Rc<Ctx>,
    covers: Covers,
    settings: settings::Store,
}

/// What hon's terminal app sets up (installed sources, the database, config and tracking),
/// plus the GUI's cover cache and settings next to the database. Must run inside the tokio
/// runtime: plugins and the tracking syncer start here.
fn start() -> anyhow::Result<Started> {
    let paths = Paths::detect()?;
    let config = Config::load(&paths)?;
    let db = Db::open(&paths.db_file)?;
    let sources = Store::new(&db, &paths.plugins_dir).load_installed()?;
    let tracking = Tracking::start(&paths).unwrap_or_else(|e| {
        eprintln!("warning: {e:#}");
        None
    });
    let data = paths
        .db_file
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    Ok(Started {
        ctx: Rc::new(Ctx {
            sources,
            db,
            config,
            tracking,
        }),
        covers: Covers::new(data.join("covers")),
        settings: settings::Store::new(&data),
    })
}

/// As in the terminal app: a panic ends hon, since Luau errors unwind through Rust frames
/// and hon can't build with `panic = "abort"`.
fn abort_on_panic() {
    let print = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        print(info);
        std::process::abort();
    }));
}

/// The screens the sidebar leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Library(Shelf),
    /// Trending manga, and search results.
    Discover,
    Settings,
}

/// Which library series a library screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shelf {
    All,
    /// Series with a chapter left partly read.
    Reading,
    Favorites,
}

/// A change on the settings screen.
#[derive(Debug, Clone, Copy)]
pub enum Pref {
    Mode(Mode),
    RightToLeft(bool),
    TwoPages(bool),
    Fit(Fit),
    Backdrop(Backdrop),
    Covers(bool),
    Sort(Sort),
    View(View),
}

#[derive(Debug, Clone)]
pub enum Message {
    Nav(Screen),
    Pref(Pref),
    ToggleFavorite(i64),
    UnreadOnly(bool),
    Help,
    Query(String),
    Search,
    Searched(u64, Arc<SearchOutcome>),
    ClearSearch,
    /// A search result, by index.
    OpenResult(usize),
    /// A library series by id; `read` opens the reader where reading left off.
    OpenEntry {
        id: i64,
        read: bool,
    },
    /// The search for a library series: its id, whether to read, and the outcome.
    Found(i64, bool, Result<Arc<SearchOutcome>, String>),
    CoverLoaded(String, Box<Cover>),
    BannerLoaded(String, Option<iced::widget::image::Handle>),
    Trending(Vec<Trend>),
    /// Searches the sources for a title picked on Discover.
    SearchFor(String),
    /// Shows a series' whole description, or folds it again.
    More,
    /// Starts moving the window.
    Drag,
    PickSource(views::SourceChoice),
    /// A chapter (index into the series' source's chapters) and the page to start on.
    Read(usize, u32),
    Continue,
    ToggleRead(ChapterNo),
    SortChapters,
    Back,
    Reader(reader::Msg),
    Key(keyboard::Event),
    Resized(Size),
    Dismiss,
}

pub struct App {
    rt: Handle,
    ctx: Result<Rc<Ctx>, String>,
    covers: Option<Covers>,
    /// Covers by key; `None` while one is being fetched.
    cover_cache: HashMap<String, Option<Cover>>,
    settings_store: Option<settings::Store>,
    prefs: Prefs,
    screen: Screen,
    /// Showing the keyboard shortcuts.
    help: bool,
    /// The series page lists unread chapters only.
    unread_only: bool,
    library: Vec<LibraryEntry>,
    query: String,
    searching: bool,
    generation: u64,
    outcome: Option<Arc<SearchOutcome>>,
    series: Option<SeriesView>,
    reader: Option<Reader>,
    window: Size,
    notes: Vec<String>,
    trending: Vec<Trend>,
    /// Keys whose banner was asked for.
    banners: HashSet<String>,
    /// The open series' description is unfolded.
    more: bool,
    /// Open the first result of the next search and resume reading it.
    read_first: bool,
    newest_first: bool,
}

pub struct SeriesView {
    pub series: Series,
    /// `None` while the series is being looked up on the sources.
    pub group: Option<TitleGroup>,
    pub failed: Option<String>,
    pub source: Option<SourceId>,
    pub progress: HashMap<ChapterNo, Progress>,
}

impl SeriesView {
    /// The chosen source's chapters, ascending.
    pub fn chapters(&self) -> &[ChapterRef] {
        match (&self.group, &self.source) {
            (Some(group), Some(source)) => {
                group.source(source).map(|s| &s.chapters[..]).unwrap_or(&[])
            }
            _ => &[],
        }
    }

    /// The chapter and page reading picks up at on the chosen source.
    pub fn resume(&self) -> Option<(usize, u32)> {
        let source = self.source.as_ref()?;
        let chapters = self.chapters();
        let numbers: Vec<ChapterNo> = chapters.iter().map(|c| c.number).collect();
        let n = resume_chapter(&numbers, &self.progress)?;
        let index = chapters.iter().position(|c| c.number == n)?;
        Some((index, resume_page(self.progress.get(&n), source)))
    }

    pub fn read_count(&self) -> usize {
        self.chapters()
            .iter()
            .filter(|c| self.progress.get(&c.number).is_some_and(|p| p.read))
            .count()
    }
}

/// The source to open a series on: the one its latest progress was saved on, if it has the
/// series, else the one with the most chapters.
fn starting_source(group: &TitleGroup, progress: &HashMap<ChapterNo, Progress>) -> SourceId {
    progress
        .values()
        .filter_map(|p| Some((p.updated_at, p.source.as_deref()?)))
        .max_by_key(|&(at, _)| at)
        .and_then(|(_, slug)| {
            group
                .sources
                .iter()
                .find(|s| s.manga.source.slug() == slug)
                .map(|s| s.manga.source.clone())
        })
        .unwrap_or_else(|| group.best_source())
}

fn open_series(db: &Db, group: TitleGroup) -> anyhow::Result<SeriesView> {
    let series = db.series(&group.keys, &group.title)?;
    let progress = db.progress(series.id)?;
    let source = starting_source(&group, &progress);
    Ok(SeriesView {
        series,
        group: Some(group),
        failed: None,
        source: Some(source),
        progress,
    })
}

/// The search result that is the library series `id`.
fn find_group(db: &Db, id: i64, outcome: &SearchOutcome) -> anyhow::Result<Option<TitleGroup>> {
    for group in &outcome.groups {
        if db.find_series(&group.keys)? == Some(id) {
            return Ok(Some(group.clone()));
        }
    }
    Ok(None)
}

/// The key a cover is cached under.
pub fn cover_key(title: &str) -> String {
    normalize_title(title)
}

impl App {
    fn boot(started: Result<Started, String>, launch: Launch, rt: Handle) -> (App, Task<Message>) {
        let (ctx, covers, store) = match started {
            Ok(s) => (Ok(s.ctx), Some(s.covers), Some(s.settings)),
            Err(e) => (Err(e), None, None),
        };
        let prefs = store.as_ref().map(|s| s.load()).unwrap_or_default();
        let search = !launch.query.is_empty();
        let mut app = App {
            rt,
            ctx,
            covers,
            cover_cache: HashMap::new(),
            settings_store: store,
            prefs,
            screen: Screen::Home,
            help: false,
            unread_only: false,
            library: Vec::new(),
            query: launch.query,
            searching: false,
            generation: 0,
            outcome: None,
            series: None,
            reader: None,
            window: WINDOW,
            notes: Vec::new(),
            trending: Vec::new(),
            banners: HashSet::new(),
            more: false,
            read_first: launch.read && search,
            newest_first: true,
        };
        let mut tasks = vec![app.refresh_library(), app.load_trending()];
        if search {
            tasks.push(app.update(Message::Search));
        }
        (app, Task::batch(tasks))
    }

    fn theme(&self) -> Theme {
        theme::theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            keyboard::listen().map(Message::Key),
            window::resize_events().map(|(_, size)| Message::Resized(size)),
        ])
    }

    fn reader_viewport(&self) -> (f32, f32) {
        (
            self.window.width,
            self.window.height - reader::BAR_H - reader::BOTTOM_H,
        )
    }

    fn refresh_library(&mut self) -> Task<Message> {
        let Ok(ctx) = &self.ctx else {
            return Task::none();
        };
        match ctx.db.library() {
            Ok(library) => self.library = library,
            Err(e) => self.notes.push(format!("{e:#}")),
        }
        let wanted: Vec<(String, Option<i64>)> = self
            .library
            .iter()
            .map(|e| (e.series.title.clone(), Some(e.series.id)))
            .collect();
        let covers = self.want_covers(wanted);
        match self.featured() {
            Some(title) => Task::batch([covers, self.want_banner(&title)]),
            None => covers,
        }
    }

    fn load_trending(&self) -> Task<Message> {
        let Some(covers) = self.covers.clone().filter(|_| self.prefs.covers) else {
            return Task::none();
        };
        let job = self.rt.spawn(async move { covers.trending().await });
        Task::perform(job, |trends| Message::Trending(trends.unwrap_or_default()))
    }

    /// Fetches a series' banner once its cover says there is one.
    fn want_banner(&mut self, title: &str) -> Task<Message> {
        let key = cover_key(title);
        let wanted = self
            .cover_cache
            .get(&key)
            .and_then(Option::as_ref)
            .is_some_and(Cover::wants_banner);
        let Some(covers) = self.covers.clone().filter(|_| wanted) else {
            return Task::none();
        };
        if !self.banners.insert(key.clone()) {
            return Task::none();
        }
        let job = self.rt.spawn({
            let key = key.clone();
            async move { covers.banner(&key).await }
        });
        Task::perform(job, move |banner| {
            Message::BannerLoaded(key.clone(), banner.ok().flatten())
        })
    }

    /// The series whose banner shows right now: the open one, else the one on Home's hero.
    fn featured(&self) -> Option<String> {
        match &self.series {
            Some(view) => Some(view.series.title.clone()),
            None => self.library.first().map(|e| e.series.title.clone()),
        }
    }

    /// Fetches the covers not asked for yet, for titles with their library series if any.
    fn want_covers(&mut self, titles: Vec<(String, Option<i64>)>) -> Task<Message> {
        let (Ok(ctx), Some(covers)) = (&self.ctx, &self.covers) else {
            return Task::none();
        };
        if !self.prefs.covers {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for (title, series) in titles {
            let key = cover_key(&title);
            if self.cover_cache.contains_key(&key) {
                continue;
            }
            self.cover_cache.insert(key.clone(), None);
            let anilist = series.and_then(|id| linked_anilist(ctx, id));
            let covers = covers.clone();
            let job = self.rt.spawn({
                let key = key.clone();
                async move { covers.get(&key, &title, anilist).await }
            });
            tasks.push(Task::perform(job, move |cover| {
                Message::CoverLoaded(key.clone(), Box::new(cover.unwrap_or_default()))
            }));
        }
        Task::batch(tasks)
    }

    pub fn cover(&self, title: &str) -> Option<&Cover> {
        self.cover_cache
            .get(&cover_key(title))
            .and_then(Option::as_ref)
    }

    /// Takes in the reader's settings, which change as you read.
    fn save_settings(&mut self) {
        let Some(reader) = &self.reader else { return };
        let settings = reader.settings();
        if settings == self.prefs.reader {
            return;
        }
        self.prefs.reader = settings;
        self.save_prefs();
    }

    fn save_prefs(&mut self) {
        if let Some(store) = &self.settings_store
            && let Err(e) = store.save(&self.prefs)
        {
            self.notes.push(format!("Couldn't save settings: {e:#}"));
        }
    }

    pub fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn is_favorite(&self, id: i64) -> bool {
        self.prefs.favorites.contains(&id)
    }

    /// The library series a shelf shows, in the chosen order.
    pub fn shelf(&self, shelf: Shelf) -> Vec<&LibraryEntry> {
        let mut entries: Vec<&LibraryEntry> = self
            .library
            .iter()
            .filter(|e| match shelf {
                Shelf::All => true,
                Shelf::Reading => !e.progress.read && e.progress.last_page.is_some(),
                Shelf::Favorites => self.is_favorite(e.series.id),
            })
            .collect();
        match self.prefs.sort {
            Sort::Recent => {}
            Sort::Title => entries.sort_by_key(|e| e.series.title.to_lowercase()),
            Sort::Progress => entries.sort_by_key(|e| std::cmp::Reverse(e.read)),
        }
        entries
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let ctx = match &self.ctx {
            Ok(ctx) => ctx.clone(),
            Err(_) => return Task::none(),
        };
        match message {
            Message::Nav(screen) => {
                self.reader = None;
                self.series = None;
                self.screen = screen;
                match screen {
                    Screen::Discover => iced::widget::operation::focus(SEARCH_INPUT),
                    _ => self.refresh_library(),
                }
            }
            Message::Pref(change) => {
                let reader = &mut self.prefs.reader;
                match change {
                    Pref::Mode(m) => reader.mode = m,
                    Pref::RightToLeft(on) => reader.right_to_left = on,
                    Pref::TwoPages(on) => reader.two_pages = on,
                    Pref::Fit(f) => reader.fit = f,
                    Pref::Backdrop(b) => reader.backdrop = b,
                    Pref::Covers(on) => self.prefs.covers = on,
                    Pref::Sort(sort) => self.prefs.sort = sort,
                    Pref::View(view) => self.prefs.view = view,
                }
                self.save_prefs();
                if matches!(change, Pref::Covers(true)) {
                    return Task::batch([self.refresh_library(), self.load_trending()]);
                }
                Task::none()
            }
            Message::ToggleFavorite(id) => {
                if !self.prefs.favorites.remove(&id) {
                    self.prefs.favorites.insert(id);
                }
                self.save_prefs();
                Task::none()
            }
            Message::UnreadOnly(on) => {
                self.unread_only = on;
                Task::none()
            }
            Message::Help => {
                self.help = !self.help;
                Task::none()
            }
            Message::Query(q) => {
                self.query = q;
                Task::none()
            }
            Message::Search => {
                let query = self.query.trim().to_string();
                if query.is_empty() {
                    return Task::none();
                }
                self.generation += 1;
                self.searching = true;
                self.series = None;
                self.screen = Screen::Discover;
                let generation = self.generation;
                let sources = ctx.sources.clone();
                let job = self
                    .rt
                    .spawn(async move { search_all(&sources, &query).await });
                Task::perform(job, move |outcome| {
                    let outcome = outcome.unwrap_or_else(|e| SearchOutcome {
                        groups: Vec::new(),
                        failures: vec![(SourceId::new("hon", "hon"), e.to_string())],
                    });
                    Message::Searched(generation, Arc::new(outcome))
                })
            }
            Message::Searched(generation, outcome) => {
                if generation != self.generation {
                    return Task::none();
                }
                self.searching = false;
                let titles = outcome
                    .groups
                    .iter()
                    .map(|g| (g.title.clone(), ctx.db.find_series(&g.keys).ok().flatten()))
                    .collect();
                self.outcome = Some(outcome);
                let covers = self.want_covers(titles);
                if !std::mem::take(&mut self.read_first) {
                    return covers;
                }
                let _ = self.update(Message::OpenResult(0));
                Task::batch([covers, self.update(Message::Continue)])
            }
            Message::ClearSearch => {
                self.outcome = None;
                self.query.clear();
                self.generation += 1;
                self.searching = false;
                Task::none()
            }
            Message::OpenResult(i) => {
                let Some(group) = self.outcome.as_ref().and_then(|o| o.groups.get(i)).cloned()
                else {
                    return Task::none();
                };
                match open_series(&ctx.db, group) {
                    Ok(view) => {
                        let title = view.series.title.clone();
                        let id = view.series.id;
                        self.series = Some(view);
                        self.more = false;
                        let cover = self.want_covers(vec![(title.clone(), Some(id))]);
                        return Task::batch([cover, self.want_banner(&title)]);
                    }
                    Err(e) => self.notes.push(format!("{e:#}")),
                }
                Task::none()
            }
            Message::OpenEntry { id, read } => {
                let Some(entry) = self.library.iter().find(|e| e.series.id == id) else {
                    return Task::none();
                };
                let series = entry.series.clone();
                let title = series.title.clone();
                self.series = Some(SeriesView {
                    series,
                    group: None,
                    failed: None,
                    source: None,
                    progress: ctx.db.progress(id).unwrap_or_default(),
                });
                self.more = false;
                let banner = self.want_banner(&title);
                let sources = ctx.sources.clone();
                let job = self
                    .rt
                    .spawn(async move { search_all(&sources, &title).await });
                let found = Task::perform(job, move |outcome| {
                    Message::Found(id, read, outcome.map(Arc::new).map_err(|e| e.to_string()))
                });
                Task::batch([banner, found])
            }
            Message::Found(id, read, outcome) => {
                let Some(view) = self.series.as_mut().filter(|v| v.series.id == id) else {
                    return Task::none();
                };
                let found = outcome.and_then(|outcome| match find_group(&ctx.db, id, &outcome) {
                    Ok(Some(group)) => Ok(group),
                    Ok(None) => Err(not_found(&view.series.title, &outcome)),
                    Err(e) => Err(format!("{e:#}")),
                });
                match found {
                    Ok(group) => {
                        view.source = Some(starting_source(&group, &view.progress));
                        view.group = Some(group);
                        if read {
                            return self.update(Message::Continue);
                        }
                    }
                    Err(e) => view.failed = Some(e),
                }
                Task::none()
            }
            Message::CoverLoaded(key, cover) => {
                self.cover_cache.insert(key, Some(*cover));
                match self.featured() {
                    Some(title) => self.want_banner(&title),
                    None => Task::none(),
                }
            }
            Message::BannerLoaded(key, banner) => {
                if let Some(Some(cover)) = self.cover_cache.get_mut(&key) {
                    cover.banner = banner;
                }
                Task::none()
            }
            Message::Trending(trends) => {
                let titles = trends.iter().map(|t| (t.title.clone(), None)).collect();
                self.trending = trends;
                self.want_covers(titles)
            }
            Message::SearchFor(title) => {
                self.query = title;
                self.update(Message::Search)
            }
            Message::More => {
                self.more = !self.more;
                Task::none()
            }
            Message::Drag => window::latest().and_then(window::drag),
            Message::PickSource(views::SourceChoice(id)) => {
                if let Some(view) = &mut self.series {
                    view.source = Some(id);
                }
                Task::none()
            }
            Message::Continue => match self.series.as_ref().and_then(SeriesView::resume) {
                Some((index, page)) => self.open_reader(&ctx, index, page),
                None => Task::none(),
            },
            Message::Read(index, page) => self.open_reader(&ctx, index, page),
            Message::ToggleRead(n) => {
                let Some(view) = &mut self.series else {
                    return Task::none();
                };
                let read = !view.progress.get(&n).is_some_and(|p| p.read);
                let result = ctx.db.set_read(view.series.id, n, read).and_then(|()| {
                    if read {
                        ctx.tracked_read(view.series.id)?;
                    }
                    ctx.db.progress(view.series.id)
                });
                match result {
                    Ok(progress) => view.progress = progress,
                    Err(e) => self.notes.push(format!("{e:#}")),
                }
                Task::none()
            }
            Message::SortChapters => {
                self.newest_first = !self.newest_first;
                Task::none()
            }
            Message::Back => self.back(&ctx),
            Message::Reader(reader::Msg::Close) => self.back(&ctx),
            Message::Reader(reader::Msg::Drag) => self.update(Message::Drag),
            Message::Reader(msg) => {
                let Some(reader) = &mut self.reader else {
                    return Task::none();
                };
                let (task, note) = reader.update(msg, &ctx, &self.rt);
                self.notes.extend(note);
                self.save_settings();
                task.map(Message::Reader)
            }
            Message::Key(event) => self.key(event, &ctx),
            Message::Resized(size) => {
                self.window = size;
                let viewport = self.reader_viewport();
                match &mut self.reader {
                    Some(reader) => reader.resize(viewport).map(Message::Reader),
                    None => Task::none(),
                }
            }
            Message::Dismiss => {
                self.notes.clear();
                Task::none()
            }
        }
    }

    fn open_reader(&mut self, ctx: &Ctx, index: usize, page: u32) -> Task<Message> {
        let Some(view) = &self.series else {
            return Task::none();
        };
        let Some(id) = &view.source else {
            return Task::none();
        };
        let Some(source) = ctx.sources.iter().find(|s| s.id() == *id).cloned() else {
            self.notes
                .push(format!("{} isn't installed anymore.", id.name()));
            return Task::none();
        };
        let (reader, task) = Reader::open(
            &self.rt,
            Opening {
                series: view.series.clone(),
                source,
                chapters: view.chapters().to_vec(),
                index,
                page,
                viewport: self.reader_viewport(),
                settings: self.prefs.reader,
                cover_color: self.cover(&view.series.title).and_then(|c| c.color),
            },
        );
        self.reader = Some(reader);
        task.map(Message::Reader)
    }

    fn back(&mut self, ctx: &Ctx) -> Task<Message> {
        if std::mem::take(&mut self.help) {
            return Task::none();
        }
        if self.reader.take().is_some() {
            if let Some(view) = &mut self.series {
                match ctx.db.progress(view.series.id) {
                    Ok(progress) => view.progress = progress,
                    Err(e) => self.notes.push(format!("{e:#}")),
                }
            }
            return Task::none();
        }
        if self.series.take().is_some() {
            return self.refresh_library();
        }
        if self.screen != Screen::Home {
            return self.update(Message::Nav(Screen::Home));
        }
        Task::none()
    }

    fn key(&mut self, event: keyboard::Event, ctx: &Ctx) -> Task<Message> {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return Task::none();
        };
        if let Key::Named(Named::Escape) = key.as_ref() {
            return self.back(ctx);
        }
        if key.as_ref() == Key::Character("?") {
            return self.update(Message::Help);
        }
        let Some(reader) = &self.reader else {
            if key.as_ref() == Key::Character("/") {
                return self.update(Message::Nav(Screen::Discover));
            }
            return Task::none();
        };
        enum Do {
            Scroll(f32),
            Send(reader::Msg),
        }
        let pages = reader.mode() == Mode::Pages;
        // Turning towards the left goes forward when reading right to left.
        let left = if reader.right_to_left() { 1 } else { -1 };
        let screen = reader.viewport_height() * 0.85;
        let step = |forward: bool, distance: f32| {
            let sign = if forward { 1 } else { -1 };
            if pages {
                Do::Send(reader::Msg::Turn(sign))
            } else {
                Do::Scroll(sign as f32 * distance)
            }
        };
        let action = match key.as_ref() {
            Key::Named(Named::Space) => Some(step(!modifiers.shift(), screen)),
            Key::Named(Named::PageDown) => Some(step(true, screen)),
            Key::Named(Named::PageUp) => Some(step(false, screen)),
            Key::Named(Named::ArrowDown) | Key::Character("j") => Some(step(true, 120.0)),
            Key::Named(Named::ArrowUp) | Key::Character("k") => Some(step(false, 120.0)),
            Key::Named(Named::ArrowLeft) if pages => Some(Do::Send(reader::Msg::Turn(left))),
            Key::Named(Named::ArrowRight) if pages => Some(Do::Send(reader::Msg::Turn(-left))),
            Key::Named(Named::ArrowRight) | Key::Character("n" | "l") => {
                reader.next().map(|i| Do::Send(reader::Msg::Chapter(i)))
            }
            Key::Named(Named::ArrowLeft) | Key::Character("p" | "h") => {
                reader.previous().map(|i| Do::Send(reader::Msg::Chapter(i)))
            }
            Key::Character("+" | "=") => Some(Do::Send(reader::Msg::Zoom(1.15))),
            Key::Character("-") => Some(Do::Send(reader::Msg::Zoom(1.0 / 1.15))),
            Key::Character("f") => Some(Do::Send(reader::Msg::ToggleBars)),
            Key::Character("t") => Some(Do::Send(reader::Msg::Thumbnails(
                !reader.settings().thumbnails,
            ))),
            Key::Character("d") => Some(Do::Send(reader::Msg::TwoPages(
                !reader.settings().two_pages,
            ))),
            Key::Named(Named::Home) => Some(Do::Send(reader::Msg::Seek(0))),
            Key::Named(Named::End) => Some(Do::Send(reader::Msg::Seek(u16::MAX))),
            Key::Character("m") => Some(Do::Send(reader::Msg::SetMode(if pages {
                Mode::Strip
            } else {
                Mode::Pages
            }))),
            _ => None,
        };
        match action {
            Some(Do::Send(msg)) => self.update(Message::Reader(msg)),
            Some(Do::Scroll(dy)) => reader.scroll_by(dy).map(Message::Reader),
            None => Task::none(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let ctx = match &self.ctx {
            Ok(ctx) => ctx,
            Err(e) => return views::failed_to_start(e),
        };
        let screen = if let Some(reader) = &self.reader {
            reader.view().map(Message::Reader)
        } else {
            let main = if let Some(view) = &self.series {
                views::series(self, view)
            } else if ctx.sources.is_empty() && self.screen != Screen::Settings {
                views::no_sources(NO_SOURCES)
            } else {
                match self.screen {
                    Screen::Home => views::home(self),
                    Screen::Library(shelf) => views::library(self, shelf),
                    Screen::Discover => views::discover(self),
                    Screen::Settings => views::settings(self, ctx),
                }
            };
            views::shell(self, ctx, main)
        };
        let screen = if self.help {
            views::help(screen)
        } else {
            screen
        };
        views::with_notes(screen, &self.notes)
    }

    pub fn library_entries(&self) -> &[LibraryEntry] {
        &self.library
    }

    pub fn searching(&self) -> bool {
        self.searching
    }

    pub fn outcome(&self) -> Option<&SearchOutcome> {
        self.outcome.as_deref()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn newest_first(&self) -> bool {
        self.newest_first
    }

    pub fn unread_only(&self) -> bool {
        self.unread_only
    }

    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self.settings_store.as_ref().and_then(|s| s.path().parent())
    }
}

/// The AniList entry the series is linked to, when logged in.
fn linked_anilist(ctx: &Ctx, series: i64) -> Option<i64> {
    let account = ctx.tracking.as_ref()?.account();
    let link = ctx
        .db
        .link(series, hon_core::track::ANILIST, &account)
        .ok()??;
    link.remote_id
}

fn not_found(title: &str, outcome: &SearchOutcome) -> String {
    let mut msg = format!("None of your sources has \"{title}\" right now.");
    if !outcome.failures.is_empty() {
        let names: Vec<&str> = outcome.failures.iter().map(|(s, _)| s.name()).collect();
        msg += &format!(" {} didn't answer.", names.join(", "));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use hon_core::model::MangaRef;
    use hon_core::model::test_ids::{alpha, beta};
    use hon_core::search::SourceSummary;

    fn chapter(source: SourceId, n: &str) -> ChapterRef {
        ChapterRef {
            source,
            id: n.into(),
            number: ChapterNo::parse(n).unwrap(),
            title: None,
            published: None,
        }
    }

    fn summary(source: SourceId, chapters: &[&str]) -> SourceSummary {
        let chapters: Vec<ChapterRef> = chapters
            .iter()
            .map(|n| chapter(source.clone(), n))
            .collect();
        SourceSummary {
            manga: MangaRef {
                source,
                id: "m".into(),
                title: "Title".into(),
                alt_titles: Vec::new(),
                url: String::new(),
            },
            latest: chapters.iter().map(|c| c.number).max(),
            gaps: Vec::new(),
            updated: None,
            chapters,
        }
    }

    fn group(keys: &[&str]) -> TitleGroup {
        TitleGroup {
            keys: keys.iter().map(|k| k.to_string()).collect(),
            title: "Title".into(),
            sources: vec![
                summary(alpha(), &["1", "2", "3"]),
                summary(beta(), &["1", "2"]),
            ],
            unavailable: Vec::new(),
        }
    }

    #[test]
    fn launch_takes_a_title_and_read() {
        let args = |a: &[&str]| Launch::parse(a.iter().map(|s| s.to_string()));
        assert_eq!(args(&[]), Launch::default());
        let launch = args(&["solo", "--read", "leveling"]);
        assert_eq!(launch.query, "solo leveling");
        assert!(launch.read);
    }

    #[test]
    fn a_new_series_opens_on_the_best_source_at_chapter_one() {
        let db = Db::open_in_memory().unwrap();
        let view = open_series(&db, group(&["title"])).unwrap();
        assert_eq!(view.source, Some(alpha()));
        assert_eq!(view.resume(), Some((0, 0)));
        assert_eq!(view.read_count(), 0);
    }

    #[test]
    fn a_series_reopens_on_the_source_and_page_it_was_left_at() {
        let db = Db::open_in_memory().unwrap();
        let series = db.series(&["title".into()], "Title").unwrap();
        let two = ChapterNo::parse("2").unwrap();
        db.set_read(series.id, ChapterNo::parse("1").unwrap(), true)
            .unwrap();
        db.save_page(series.id, two, &beta(), 7, 20).unwrap();
        let view = open_series(&db, group(&["title"])).unwrap();
        assert_eq!(view.source, Some(beta()));
        assert_eq!(view.resume(), Some((1, 7)));
        assert_eq!(view.read_count(), 1);
    }

    #[test]
    fn a_library_series_is_found_among_results_by_any_of_its_titles() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .series(&["old title".into(), "title".into()], "Old Title")
            .unwrap()
            .id;
        let outcome = SearchOutcome {
            groups: vec![group(&["something else"]), group(&["new title", "title"])],
            failures: Vec::new(),
        };
        let found = find_group(&db, id, &outcome).unwrap().unwrap();
        assert_eq!(found.keys[0], "new title");
        let none = SearchOutcome {
            groups: vec![group(&["something else"])],
            failures: vec![(beta(), "timed out".into())],
        };
        assert!(find_group(&db, id, &none).unwrap().is_none());
        assert!(not_found("Old Title", &none).contains("Beta didn't answer"));
    }
}
