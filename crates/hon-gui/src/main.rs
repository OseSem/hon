//! hon's desktop reader: search every installed source, pick a series and chapter, and read
//! it as a long strip. Progress goes to the same database as the terminal app, so either one
//! picks up where the other left off.

mod reader;
mod strip;

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use hon::commands::track::Tracking;
use hon::commands::{Ctx, NO_SOURCES};
use hon::config::{Config, Paths};
use hon_core::db::{Db, Progress, Series, resume_chapter, resume_page};
use hon_core::model::{ChapterNo, ChapterRef, SourceId};
use hon_core::plugins::store::Store;
use hon_core::search::{SearchOutcome, TitleGroup, search_all};
use hon_core::sources::Source;
use iced::keyboard::{self, Key, key::Named};
use iced::widget::{
    Column, button, center, column, container, pick_list, row, rule, scrollable, text, text_input,
};
use iced::{Alignment, Element, Length, Size, Subscription, Task, Theme, window};
use tokio::runtime::Handle;

use crate::reader::Reader;

const WINDOW: Size = Size::new(1100.0, 900.0);
/// Height of the reader's top bar, taken off the window for its viewport.
const BAR_H: f32 = 40.0;
const SEARCH_INPUT: &str = "search";

fn main() -> iced::Result {
    abort_on_panic();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let started = {
        let _inside = rt.enter();
        context().map(Rc::new).map_err(|e| format!("{e:#}"))
    };
    let ctx = started.as_ref().ok().cloned();
    let boot = Cell::new(Some((started, Launch::parse(std::env::args().skip(1)))));
    let handle = rt.handle().clone();
    let result = iced::application(
        move || {
            let (ctx, launch) = boot.take().expect("booted once");
            App::boot(ctx, launch, handle.clone())
        },
        App::update,
        App::view,
    )
    .title("hon")
    .window_size(WINDOW)
    .theme(App::theme)
    .subscription(App::subscription)
    .run();
    if let Some(ctx) = ctx {
        rt.block_on(ctx.finish_tracking());
    }
    result
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

/// What hon's terminal app sets up: installed sources, the database, config and tracking.
/// Must run inside the tokio runtime: plugins and the tracking syncer start here.
fn context() -> anyhow::Result<Ctx> {
    let paths = Paths::detect()?;
    let config = Config::load(&paths)?;
    let db = Db::open(&paths.db_file)?;
    let sources = Store::new(&db, &paths.plugins_dir).load_installed()?;
    let tracking = Tracking::start(&paths).unwrap_or_else(|e| {
        eprintln!("warning: {e:#}");
        None
    });
    Ok(Ctx {
        sources,
        db,
        config,
        tracking,
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

#[derive(Debug, Clone)]
enum Message {
    Query(String),
    Search,
    Searched(u64, Arc<SearchOutcome>),
    OpenSeries(usize),
    PickSource(SourceChoice),
    /// A chapter (index into the series' source's chapters) and the page to start on.
    Read(usize, u32),
    Back,
    Reader(reader::Msg),
    Key(keyboard::Event),
    Resized(Size),
    Dismiss,
}

/// A source in the series' source picker.
#[derive(Debug, Clone, PartialEq)]
struct SourceChoice(SourceId);

impl std::fmt::Display for SourceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.name())
    }
}

struct App {
    rt: Handle,
    ctx: Result<Rc<Ctx>, String>,
    query: String,
    searching: bool,
    generation: u64,
    outcome: Option<Arc<SearchOutcome>>,
    series: Option<SeriesView>,
    reader: Option<Reader>,
    window: Size,
    notes: Vec<String>,
    /// Open the first result of the next search and resume reading it.
    read_first: bool,
}

struct SeriesView {
    group: TitleGroup,
    series: Series,
    source: SourceId,
    progress: HashMap<ChapterNo, Progress>,
}

impl SeriesView {
    /// The chosen source's chapters, ascending.
    fn chapters(&self) -> &[ChapterRef] {
        self.group
            .source(&self.source)
            .map(|s| &s.chapters[..])
            .unwrap_or(&[])
    }

    /// The chapter and page reading picks up at on the chosen source.
    fn resume(&self) -> Option<(usize, u32)> {
        let chapters = self.chapters();
        let numbers: Vec<ChapterNo> = chapters.iter().map(|c| c.number).collect();
        let n = resume_chapter(&numbers, &self.progress)?;
        let index = chapters.iter().position(|c| c.number == n)?;
        Some((index, resume_page(self.progress.get(&n), &self.source)))
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

impl App {
    fn boot(ctx: Result<Rc<Ctx>, String>, launch: Launch, rt: Handle) -> (App, Task<Message>) {
        let search = !launch.query.is_empty();
        let mut app = App {
            rt,
            ctx,
            query: launch.query,
            searching: false,
            generation: 0,
            outcome: None,
            series: None,
            reader: None,
            window: WINDOW,
            notes: Vec::new(),
            read_first: launch.read && search,
        };
        let focus = iced::widget::operation::focus(SEARCH_INPUT);
        if !search {
            return (app, focus);
        }
        let task = app.update(Message::Search);
        (app, Task::batch([focus, task]))
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            keyboard::listen().map(Message::Key),
            window::resize_events().map(|(_, size)| Message::Resized(size)),
        ])
    }

    fn reader_viewport(&self) -> (f32, f32) {
        (self.window.width, self.window.height - BAR_H)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let ctx = match &self.ctx {
            Ok(ctx) => ctx.clone(),
            Err(_) => return Task::none(),
        };
        match message {
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
                let generation = self.generation;
                let sources = ctx.sources.clone();
                let job = self
                    .rt
                    .spawn(async move { search_all(&sources, &query).await });
                Task::perform(job, move |outcome| match outcome {
                    Ok(o) => Message::Searched(generation, Arc::new(o)),
                    Err(e) => Message::Searched(
                        generation,
                        Arc::new(SearchOutcome {
                            groups: Vec::new(),
                            failures: vec![(SourceId::new("hon", "hon"), e.to_string())],
                        }),
                    ),
                })
            }
            Message::Searched(generation, outcome) => {
                if generation != self.generation {
                    return Task::none();
                }
                self.searching = false;
                self.outcome = Some(outcome);
                if !std::mem::take(&mut self.read_first) {
                    return Task::none();
                }
                let _ = self.update(Message::OpenSeries(0));
                match self.series.as_ref().and_then(SeriesView::resume) {
                    Some((index, page)) => self.update(Message::Read(index, page)),
                    None => Task::none(),
                }
            }
            Message::OpenSeries(i) => {
                let Some(group) = self.outcome.as_ref().and_then(|o| o.groups.get(i)).cloned()
                else {
                    return Task::none();
                };
                match open_series(&ctx.db, group) {
                    Ok(view) => self.series = Some(view),
                    Err(e) => self.notes.push(format!("{e:#}")),
                }
                Task::none()
            }
            Message::PickSource(SourceChoice(id)) => {
                if let Some(view) = &mut self.series {
                    view.source = id;
                }
                Task::none()
            }
            Message::Read(index, page) => self.open_reader(&ctx, index, page),
            Message::Back => self.back(&ctx),
            Message::Reader(msg) => {
                let Some(reader) = &mut self.reader else {
                    return Task::none();
                };
                let (task, note) = reader.update(msg, &ctx, &self.rt);
                self.notes.extend(note);
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
        let Some(source) = ctx.sources.iter().find(|s| s.id() == view.source).cloned() else {
            self.notes
                .push(format!("{} isn't installed anymore", view.source.name()));
            return Task::none();
        };
        let (reader, task) = Reader::open(
            &self.rt,
            view.series.clone(),
            source as Arc<dyn Source>,
            view.chapters().to_vec(),
            index,
            page,
            self.reader_viewport(),
        );
        self.reader = Some(reader);
        task.map(Message::Reader)
    }

    fn back(&mut self, ctx: &Ctx) -> Task<Message> {
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
            return iced::widget::operation::focus(SEARCH_INPUT);
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
        let Some(reader) = &self.reader else {
            return Task::none();
        };
        let page = reader.viewport_height() * 0.85;
        let msg = match key.as_ref() {
            Key::Named(Named::Space) if modifiers.shift() => Some(Err(-page)),
            Key::Named(Named::Space | Named::PageDown) => Some(Err(page)),
            Key::Named(Named::PageUp) => Some(Err(-page)),
            Key::Named(Named::ArrowDown) | Key::Character("j") => Some(Err(120.0)),
            Key::Named(Named::ArrowUp) | Key::Character("k") => Some(Err(-120.0)),
            Key::Named(Named::ArrowRight) | Key::Character("n" | "l") => {
                reader.next().map(|i| Ok(reader::Msg::Chapter(i)))
            }
            Key::Named(Named::ArrowLeft) | Key::Character("p" | "h") => {
                reader.previous().map(|i| Ok(reader::Msg::Chapter(i)))
            }
            Key::Character("+" | "=") => Some(Ok(reader::Msg::Zoom(1.15))),
            Key::Character("-") => Some(Ok(reader::Msg::Zoom(1.0 / 1.15))),
            _ => None,
        };
        match msg {
            Some(Ok(msg)) => self.update(Message::Reader(msg)),
            Some(Err(dy)) => reader.scroll_by(dy).map(Message::Reader),
            None => Task::none(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let ctx = match &self.ctx {
            Ok(ctx) => ctx,
            Err(e) => return center(text(format!("hon couldn't start: {e}"))).into(),
        };
        let screen = if let Some(reader) = &self.reader {
            reader.view().map(Message::Reader)
        } else if let Some(view) = &self.series {
            series_view(view)
        } else {
            self.search_view(ctx)
        };
        if self.notes.is_empty() {
            return screen;
        }
        let notes = row![
            text(self.notes.join("\n")).size(13).width(Length::Fill),
            button(text("Dismiss").size(13)).on_press(Message::Dismiss),
        ]
        .spacing(12)
        .padding([8, 12])
        .align_y(Alignment::Center);
        column![
            container(screen).height(Length::Fill),
            rule::horizontal(1),
            notes
        ]
        .into()
    }

    fn search_view(&self, ctx: &Ctx) -> Element<'_, Message> {
        let input = text_input("Search manga across your sources", &self.query)
            .id(SEARCH_INPUT)
            .on_input(Message::Query)
            .on_submit(Message::Search)
            .padding(10)
            .size(16);
        let search = button(text("Search"))
            .on_press(Message::Search)
            .padding([10, 16]);
        let mut page = column![row![input, search].spacing(8)]
            .spacing(16)
            .padding(24);

        if ctx.sources.is_empty() {
            return page.push(text(NO_SOURCES).size(14)).into();
        }
        if self.searching {
            page = page.push(text("Searching…").size(14));
        }
        let Some(outcome) = &self.outcome else {
            return page.into();
        };
        for (source, error) in &outcome.failures {
            page = page.push(text(format!("{} failed: {error}", source.name())).size(12));
        }
        if outcome.groups.is_empty() && !self.searching {
            page = page.push(text("Nothing found.").size(14));
        }
        let mut results = Column::new().spacing(4);
        for (i, group) in outcome.groups.iter().enumerate() {
            let names: Vec<&str> = group
                .sources
                .iter()
                .map(|s| s.manga.source.name())
                .collect();
            let mut meta = names.join(", ");
            if let Some(latest) = group.sources.first().and_then(|s| s.latest) {
                meta += &format!("  ·  latest Ch {latest}");
            }
            if !group.unavailable.is_empty() {
                meta += &format!("  ·  {} source(s) didn't answer", group.unavailable.len());
            }
            let item = column![text(&group.title).size(17), text(meta).size(12)].spacing(2);
            results = results.push(
                button(item)
                    .on_press(Message::OpenSeries(i))
                    .width(Length::Fill)
                    .padding([10, 12])
                    .style(button::subtle),
            );
        }
        page.push(scrollable(results).height(Length::Fill)).into()
    }
}

fn open_series(db: &Db, group: TitleGroup) -> anyhow::Result<SeriesView> {
    let series = db.series(&group.keys, &group.title)?;
    let progress = db.progress(series.id)?;
    let source = starting_source(&group, &progress);
    Ok(SeriesView {
        group,
        series,
        source,
        progress,
    })
}

fn series_view(view: &SeriesView) -> Element<'_, Message> {
    let choices: Vec<SourceChoice> = view
        .group
        .sources
        .iter()
        .map(|s| SourceChoice(s.manga.source.clone()))
        .collect();
    let picker = pick_list(
        choices,
        Some(SourceChoice(view.source.clone())),
        Message::PickSource,
    );
    let chapters = view.chapters();
    let resume = view.resume().map(|(i, page)| {
        let mut label = format!("Continue  Ch {}", chapters[i].number);
        if page > 0 {
            label += &format!(", page {}", page + 1);
        }
        button(text(label))
            .on_press(Message::Read(i, page))
            .padding([8, 16])
    });
    let mut controls = row![
        text("Source").size(13),
        picker,
        text(format!("{} chapters", chapters.len())).size(13)
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    if let Some(resume) = resume {
        controls = controls.push(resume);
    }
    let header = column![
        row![
            button(text("‹ Search").size(13))
                .on_press(Message::Back)
                .padding([4, 10]),
            text(&view.group.title).size(22).width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        controls,
    ]
    .spacing(12);

    let mut list = Column::new().spacing(2);
    for (i, ch) in chapters.iter().enumerate().rev() {
        let progress = view.progress.get(&ch.number);
        let status = match progress {
            Some(p) if p.read => "read".to_string(),
            Some(p) if p.source.as_deref() == Some(view.source.slug()) => {
                match (p.last_page, p.page_count) {
                    (Some(page), Some(count)) => format!("page {}/{count}", page + 1),
                    _ => String::new(),
                }
            }
            _ => String::new(),
        };
        let read = progress.is_some_and(|p| p.read);
        let date = ch
            .published
            .map(|t| t.strftime("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let label = row![
            text(format!("Ch {}", ch.number)).width(90).style(if read {
                text::secondary
            } else {
                text::default
            }),
            text(ch.title.clone().unwrap_or_default())
                .width(Length::Fill)
                .style(if read { text::secondary } else { text::default }),
            text(status).size(12).style(text::secondary),
            text(date).size(12).width(90).style(text::secondary),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        let page = if read {
            0
        } else {
            resume_page(progress, &view.source)
        };
        list = list.push(
            button(label)
                .on_press(Message::Read(i, page))
                .width(Length::Fill)
                .padding([8, 12])
                .style(button::subtle),
        );
    }
    column![header, scrollable(list).height(Length::Fill)]
        .spacing(16)
        .padding(24)
        .into()
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

    fn group() -> TitleGroup {
        TitleGroup {
            keys: vec!["title".into()],
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
        let view = open_series(&db, group()).unwrap();
        assert_eq!(view.source, alpha());
        assert_eq!(view.resume(), Some((0, 0)));
    }

    #[test]
    fn a_series_reopens_on_the_source_and_page_it_was_left_at() {
        let db = Db::open_in_memory().unwrap();
        let series = db.series(&["title".into()], "Title").unwrap();
        let two = ChapterNo::parse("2").unwrap();
        db.set_read(series.id, ChapterNo::parse("1").unwrap(), true)
            .unwrap();
        db.save_page(series.id, two, &beta(), 7, 20).unwrap();
        let view = open_series(&db, group()).unwrap();
        assert_eq!(view.source, beta());
        assert_eq!(view.resume(), Some((1, 7)));
    }
}
