//! The reader: lists a chapter's pages, fetches them in reading order, keeps only the pages
//! near what's on screen decoded, and saves progress as you read. Pages show as one long
//! strip or one at a time.

use std::io::Cursor;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use hon::commands::Ctx;
use hon_core::db::Series;
use hon_core::model::{ChapterRef, PageRef, SourceId};
use hon_core::sources::Source;
use iced::widget::image::{self, FilterMethod};
use iced::widget::operation::{self, AbsoluteOffset};
use iced::widget::{
    Column, button, center, column, container, mouse_area, pick_list, row, scrollable, slider,
    space, stack, text,
};
use iced::{Alignment, Color, ContentFit, Element, Length, Task, mouse, padding};
use tokio::runtime::Handle;

use crate::settings::{Mode, Settings};
use crate::strip::Strip;
use crate::theme;

pub const STRIP_ID: &str = "reader-strip";
/// Height of the bar above the pages, progress line included.
pub const BAR_H: f32 = 50.0;
/// Height of the page slider bar under the pages.
pub const BOTTOM_H: f32 = 44.0;
const THUMB_W: f32 = 96.0;
/// Page width at zoom 1, in logical pixels.
const BASE_WIDTH: f32 = 860.0;
/// Pages fetched at once.
const FETCH_AHEAD: usize = 4;
/// Decoded pages are capped at this many device pixels per logical pixel of display width.
const MAX_SCALE: f32 = 2.0;

/// A page's bytes and its size in pixels.
type Fetched = (Arc<Vec<u8>>, (u32, u32));

#[derive(Debug, Clone)]
pub enum Msg {
    Listed(u64, Result<Arc<Vec<PageRef>>, String>),
    Fetched(u64, usize, Result<Fetched, String>),
    Decoded(u64, usize, Result<image::Handle, String>),
    Thumb(u64, usize, Result<image::Handle, String>),
    /// Jumps to a page.
    Seek(u16),
    Scrolled(scrollable::Viewport),
    Chapter(usize),
    PickChapter(ChapterChoice),
    /// Turns this many pages forward (back when negative) in page mode.
    Turn(i32),
    SetMode(Mode),
    RightToLeft(bool),
    TwoPages(bool),
    Thumbnails(bool),
    /// Shows or hides the bars around the pages.
    ToggleBars,
    Zoom(f32),
    Retry,
    Close,
}

/// A chapter in the reader's chapter picker.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterChoice {
    index: usize,
    label: String,
}

impl std::fmt::Display for ChapterChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

enum Load {
    Listing,
    Ready,
    Failed(String),
}

#[derive(Default)]
struct Slot {
    bytes: Option<Arc<Vec<u8>>>,
    image: Option<image::Handle>,
    decoding: bool,
    error: Option<String>,
    thumb: Option<image::Handle>,
    thumbing: bool,
}

pub struct Reader {
    pub series: Series,
    pub source_id: SourceId,
    source: Arc<dyn Source>,
    /// The source's chapters, ascending.
    chapters: Vec<ChapterRef>,
    choices: Vec<ChapterChoice>,
    index: usize,
    /// Bumped on every chapter change so late results of the previous one are dropped.
    generation: u64,
    load: Load,
    strip: Strip,
    slots: Vec<Slot>,
    offset: f32,
    /// The page shown in page mode; the first of the two with two pages.
    page: usize,
    bars: bool,
    viewport: (f32, f32),
    settings: Settings,
    accent: Color,
    resume_page: u32,
    current: Option<usize>,
    marked_read: bool,
    /// Dropping it stops the running fetch.
    _fetch: Option<iced::task::Handle>,
}

pub struct Opening {
    pub series: Series,
    pub source: Arc<dyn Source>,
    pub chapters: Vec<ChapterRef>,
    pub index: usize,
    pub page: u32,
    pub viewport: (f32, f32),
    pub settings: Settings,
    pub cover_color: Option<Color>,
}

impl Reader {
    pub fn open(rt: &Handle, o: Opening) -> (Reader, Task<Msg>) {
        let choices = o
            .chapters
            .iter()
            .enumerate()
            .map(|(index, c)| ChapterChoice {
                index,
                label: chapter_label(c),
            })
            .collect();
        let mut reader = Reader {
            series: o.series,
            source_id: o.source.id(),
            source: o.source,
            chapters: o.chapters,
            choices,
            index: o.index,
            generation: 0,
            load: Load::Listing,
            strip: Strip::new(0, BASE_WIDTH),
            slots: Vec::new(),
            offset: 0.0,
            page: 0,
            bars: true,
            viewport: o.viewport,
            settings: o.settings,
            accent: theme::accent(o.cover_color),
            resume_page: o.page,
            current: None,
            marked_read: false,
            _fetch: None,
        };
        let task = reader.list(rt);
        (reader, task)
    }

    pub fn chapter(&self) -> &ChapterRef {
        &self.chapters[self.index]
    }

    pub fn settings(&self) -> Settings {
        self.settings
    }

    pub fn mode(&self) -> Mode {
        self.settings.mode
    }

    fn page_width(&self) -> f32 {
        (BASE_WIDTH * self.settings.zoom)
            .min(self.viewport.0 - 24.0)
            .max(200.0)
    }

    fn list(&mut self, rt: &Handle) -> Task<Msg> {
        self.generation += 1;
        self.load = Load::Listing;
        self.slots.clear();
        self.strip = Strip::new(0, self.page_width());
        self.offset = 0.0;
        self.page = 0;
        self.current = None;
        self.marked_read = false;
        self._fetch = None;
        let generation = self.generation;
        let source = self.source.clone();
        let chapter = self.chapter().clone();
        let job = rt.spawn(async move { source.pages(&chapter).await });
        Task::perform(job, move |r| {
            let r = match r {
                Ok(Ok(pages)) if pages.is_empty() => Err("the source listed no pages".into()),
                Ok(Ok(pages)) => Ok(Arc::new(pages)),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            Msg::Listed(generation, r)
        })
    }

    /// Handles a message. Returns the follow-up work and a note to show, if any.
    pub fn update(&mut self, msg: Msg, ctx: &Ctx, rt: &Handle) -> (Task<Msg>, Option<String>) {
        let note = |r: anyhow::Result<()>| r.err().map(|e| format!("{e:#}"));
        match msg {
            Msg::Listed(g, _) | Msg::Fetched(g, ..) | Msg::Decoded(g, ..) | Msg::Thumb(g, ..)
                if g != self.generation =>
            {
                (Task::none(), None)
            }
            Msg::Listed(_, Err(e)) => {
                self.load = Load::Failed(e);
                (Task::none(), None)
            }
            Msg::Listed(_, Ok(pages)) => {
                self.load = Load::Ready;
                self.strip = Strip::new(pages.len(), self.page_width());
                self.slots = (0..pages.len()).map(|_| Slot::default()).collect();
                let start = (self.resume_page as usize).min(pages.len() - 1);
                self.resume_page = 0;
                self.page = start;
                self.offset = self.strip.top(start);
                let fetch = self.fetch(rt, pages, start);
                let scroll = self.scroll_to_offset();
                let note = note(self.track(ctx));
                (Task::batch([fetch, scroll]), note)
            }
            Msg::Fetched(_, i, Err(e)) => {
                self.slots[i].error = Some(e);
                (Task::none(), None)
            }
            Msg::Fetched(_, i, Ok((bytes, size))) => {
                self.slots[i].bytes = Some(bytes);
                let delta = self.strip.set_size(i, size, self.offset);
                let mut tasks = vec![self.decode_window(rt), self.thumbs(rt)];
                if delta != 0.0 && self.mode() == Mode::Strip {
                    self.offset += delta;
                    tasks.push(self.scroll_to_offset());
                }
                (Task::batch(tasks), None)
            }
            Msg::Decoded(_, i, result) => {
                self.slots[i].decoding = false;
                match result {
                    Ok(handle) if self.window().contains(&i) => self.slots[i].image = Some(handle),
                    Ok(_) => {}
                    Err(e) => self.slots[i].error = Some(e),
                }
                (Task::none(), None)
            }
            Msg::Thumb(_, i, result) => {
                self.slots[i].thumbing = false;
                self.slots[i].thumb = result.ok();
                (Task::none(), None)
            }
            Msg::Seek(page) => {
                if self.slots.is_empty() {
                    return (Task::none(), None);
                }
                let page = (page as usize).min(self.slots.len() - 1);
                let mut tasks = Vec::new();
                match self.mode() {
                    Mode::Strip => {
                        self.offset = self.strip.top(page);
                        tasks.push(self.scroll_to_offset());
                    }
                    Mode::Pages => self.page = self.spread_of(page).start,
                }
                let note = note(self.track(ctx));
                tasks.push(self.decode_window(rt));
                (Task::batch(tasks), note)
            }
            Msg::TwoPages(on) => {
                self.settings.two_pages = on;
                self.page = self.spread_of(self.page).start;
                (self.decode_window(rt), None)
            }
            Msg::Thumbnails(on) => {
                self.settings.thumbnails = on;
                (self.thumbs(rt), None)
            }
            Msg::ToggleBars => {
                self.bars = !self.bars;
                (Task::none(), None)
            }
            Msg::Scrolled(viewport) => {
                if self.mode() != Mode::Strip {
                    return (Task::none(), None);
                }
                self.offset = viewport.absolute_offset().y;
                let bounds = viewport.bounds();
                let task = self.resize((bounds.width, bounds.height));
                let note = note(self.track(ctx));
                (Task::batch([task, self.decode_window(rt)]), note)
            }
            Msg::Turn(by) => self.turn(by, ctx, rt),
            Msg::Chapter(index) => self.go_to_chapter(index, ctx, rt),
            Msg::PickChapter(choice) => self.go_to_chapter(choice.index, ctx, rt),
            Msg::SetMode(mode) => {
                if mode == self.mode() {
                    return (Task::none(), None);
                }
                let at = self.current_page();
                self.settings.mode = mode;
                let mut tasks = vec![];
                match mode {
                    Mode::Pages => self.page = at,
                    Mode::Strip => {
                        self.offset = self.strip.top(at);
                        tasks.push(self.scroll_to_offset());
                    }
                }
                tasks.push(self.decode_window(rt));
                (Task::batch(tasks), None)
            }
            Msg::RightToLeft(on) => {
                self.settings.right_to_left = on;
                (Task::none(), None)
            }
            Msg::Zoom(factor) => {
                self.settings.zoom = (self.settings.zoom * factor).clamp(0.4, 3.0);
                let viewport = self.viewport;
                self.viewport = (0.0, 0.0);
                (
                    Task::batch([self.resize(viewport), self.decode_window(rt)]),
                    None,
                )
            }
            Msg::Retry => (self.list(rt), None),
            // The app closes the reader.
            Msg::Close => (Task::none(), None),
        }
    }

    fn go_to_chapter(
        &mut self,
        index: usize,
        ctx: &Ctx,
        rt: &Handle,
    ) -> (Task<Msg>, Option<String>) {
        let forward = index > self.index;
        let note = if forward {
            self.mark_read(ctx).err().map(|e| format!("{e:#}"))
        } else {
            None
        };
        self.index = index.min(self.chapters.len() - 1);
        (self.list(rt), note)
    }

    /// Turns pages in page mode; turning past the last page opens the next chapter.
    fn turn(&mut self, by: i32, ctx: &Ctx, rt: &Handle) -> (Task<Msg>, Option<String>) {
        if self.slots.is_empty() {
            return (Task::none(), None);
        }
        let spreads = self.spreads();
        let at = spreads
            .iter()
            .position(|r| r.contains(&self.page))
            .unwrap_or(0);
        let target = at as i64 + by as i64;
        if target >= spreads.len() as i64 {
            return match self.next() {
                Some(next) => self.go_to_chapter(next, ctx, rt),
                None => (Task::none(), None),
            };
        }
        self.page = spreads[target.max(0) as usize].start;
        let note = self.track(ctx).err().map(|e| format!("{e:#}"));
        (self.decode_window(rt), note)
    }

    fn scroll_to_offset(&self) -> Task<Msg> {
        operation::scroll_to(
            STRIP_ID,
            AbsoluteOffset {
                x: 0.0,
                y: self.offset,
            },
        )
    }

    /// Takes in a new viewport size, keeping the same spot of the page in view.
    pub fn resize(&mut self, viewport: (f32, f32)) -> Task<Msg> {
        if viewport == self.viewport {
            return Task::none();
        }
        self.viewport = viewport;
        let width = self.page_width();
        if (width - self.strip.width()).abs() < 0.5 {
            return Task::none();
        }
        self.offset = self.strip.set_width(width, self.offset);
        // Decoded sizes depend on the width.
        for slot in &mut self.slots {
            slot.image = None;
        }
        self.scroll_to_offset()
    }

    pub fn scroll_by(&self, dy: f32) -> Task<Msg> {
        operation::scroll_by(STRIP_ID, AbsoluteOffset { x: 0.0, y: dy })
    }

    pub fn viewport_height(&self) -> f32 {
        self.viewport.1
    }

    pub fn right_to_left(&self) -> bool {
        self.settings.right_to_left
    }

    pub fn next(&self) -> Option<usize> {
        (self.index + 1 < self.chapters.len()).then_some(self.index + 1)
    }

    pub fn previous(&self) -> Option<usize> {
        self.index.checked_sub(1)
    }

    /// The pages shown together in page mode: one, or two with two pages on.
    fn spreads(&self) -> Vec<std::ops::Range<usize>> {
        if self.settings.two_pages {
            self.strip.spreads()
        } else {
            (0..self.slots.len()).map(|i| i..i + 1).collect()
        }
    }

    fn spread_of(&self, page: usize) -> std::ops::Range<usize> {
        self.spreads()
            .into_iter()
            .find(|r| r.contains(&page))
            .unwrap_or(page..page + 1)
    }

    fn current_page(&self) -> usize {
        match self.mode() {
            Mode::Strip => self.strip.current(self.offset, self.viewport.1),
            Mode::Pages => self.page,
        }
    }

    /// The pages worth keeping decoded.
    fn window(&self) -> std::ops::Range<usize> {
        match self.mode() {
            Mode::Strip => self.strip.window(self.offset, self.viewport.1),
            Mode::Pages => {
                let spread = self.spread_of(self.page);
                spread.start.saturating_sub(1)..(spread.end + 2).min(self.slots.len())
            }
        }
    }

    /// Fetches every page, starting at `start` and going on in reading order, then the
    /// ones before it, `FETCH_AHEAD` at a time.
    fn fetch(&mut self, rt: &Handle, pages: Arc<Vec<PageRef>>, start: usize) -> Task<Msg> {
        let generation = self.generation;
        let source = self.source.clone();
        let (mut tx, rx) = futures::channel::mpsc::channel(FETCH_AHEAD);
        rt.spawn(async move {
            let order: Vec<usize> = (start..pages.len()).chain((0..start).rev()).collect();
            let mut results = futures::stream::iter(order)
                .map(|i| {
                    let source = source.clone();
                    let page = pages[i].clone();
                    async move { (i, fetch_page(source.as_ref(), &page).await) }
                })
                .buffered(FETCH_AHEAD);
            while let Some(result) = results.next().await {
                if tx.send(result).await.is_err() {
                    return;
                }
            }
        });
        let (task, handle) =
            Task::run(rx, move |(i, r)| Msg::Fetched(generation, i, r)).abortable();
        self._fetch = Some(handle.abort_on_drop());
        task
    }

    /// Decodes the fetched pages near what's on screen and drops the decoded ones away
    /// from it.
    fn decode_window(&mut self, rt: &Handle) -> Task<Msg> {
        let window = self.window();
        let max_width = match self.mode() {
            Mode::Strip => self.strip.width(),
            Mode::Pages => self.viewport.0,
        };
        let max_width = (max_width * MAX_SCALE) as u32;
        let mut tasks = Vec::new();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if !window.contains(&i) {
                slot.image = None;
                continue;
            }
            let Some(bytes) = slot.bytes.clone() else {
                continue;
            };
            if slot.image.is_some() || slot.decoding || slot.error.is_some() {
                continue;
            }
            slot.decoding = true;
            let generation = self.generation;
            let job = rt.spawn_blocking(move || decode(&bytes, max_width));
            tasks.push(Task::perform(job, move |r| {
                let r = r
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| format!("{e:#}")));
                Msg::Decoded(generation, i, r)
            }));
        }
        Task::batch(tasks)
    }

    /// Makes the missing thumbnails of fetched pages, while the thumbnails show.
    fn thumbs(&mut self, rt: &Handle) -> Task<Msg> {
        if !self.settings.thumbnails {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let Some(bytes) = slot.bytes.clone() else {
                continue;
            };
            if slot.thumb.is_some() || slot.thumbing {
                continue;
            }
            slot.thumbing = true;
            let generation = self.generation;
            let job = rt.spawn_blocking(move || decode(&bytes, (THUMB_W * MAX_SCALE) as u32));
            tasks.push(Task::perform(job, move |r| {
                let r = r
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| format!("{e:#}")));
                Msg::Thumb(generation, i, r)
            }));
        }
        Task::batch(tasks)
    }

    /// Saves the page being read, and marks the chapter read once its end is on screen.
    fn track(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        if self.slots.is_empty() {
            return Ok(());
        }
        let page = self.current_page();
        if self.current != Some(page) {
            self.current = Some(page);
            let total = self.slots.len() as u32;
            let number = self.chapter().number;
            ctx.db
                .save_page(self.series.id, number, &self.source_id, page as u32, total)?;
        }
        let at_end = match self.mode() {
            Mode::Strip => self.strip.at_end(self.offset, self.viewport.1),
            Mode::Pages => self.spread_of(page).end == self.slots.len(),
        };
        if at_end {
            self.mark_read(ctx)?;
        }
        Ok(())
    }

    fn mark_read(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        if self.marked_read || self.slots.is_empty() {
            return Ok(());
        }
        self.marked_read = true;
        ctx.db
            .set_read(self.series.id, self.chapter().number, true)?;
        ctx.tracked_read(self.series.id)
    }

    pub fn view(&self) -> Element<'_, Msg> {
        let body: Element<'_, Msg> = match &self.load {
            Load::Listing => center(text("Loading pages").size(14).color(theme::MUTED)).into(),
            Load::Failed(e) => center(
                column![
                    text("This chapter's pages didn't load")
                        .font(theme::DISPLAY)
                        .size(22),
                    text(e.clone()).size(13).color(theme::MUTED),
                    button(text("Try again"))
                        .on_press(Msg::Retry)
                        .padding([8, 18])
                        .style(theme::paper),
                ]
                .spacing(12)
                .max_width(560)
                .align_x(Alignment::Center),
            )
            .into(),
            Load::Ready => match self.mode() {
                Mode::Strip => self.strip_view(),
                Mode::Pages => self.page_view(),
            },
        };
        let body: Element<'_, Msg> = if self.settings.thumbnails && matches!(self.load, Load::Ready)
        {
            row![self.thumbnails(), body].into()
        } else {
            body
        };
        if !self.bars {
            return column![self.progress_line(), body].into();
        }
        column![self.bar(), self.progress_line(), body, self.bottom()].into()
    }

    fn bottom(&self) -> Element<'_, Msg> {
        let total = self.slots.len();
        let current = self.current.unwrap_or(0);
        let counter = match (&self.load, self.mode()) {
            (Load::Ready, Mode::Pages) => {
                let spread = self.spread_of(current);
                if spread.len() > 1 {
                    format!("{}–{} of {total}", spread.start + 1, spread.end)
                } else {
                    format!("{} of {total}", current + 1)
                }
            }
            (Load::Ready, Mode::Strip) => format!("{} of {total}", current + 1),
            _ => String::new(),
        };
        let seek: Element<'_, Msg> = if total > 1 {
            slider(0..=(total - 1) as u16, current as u16, Msg::Seek)
                .width(Length::Fill)
                .style(theme::seek(self.accent))
                .into()
        } else {
            space().width(Length::Fill).into()
        };
        row![
            small("Previous chapter", self.previous().map(Msg::Chapter)),
            text(counter).size(12).color(theme::MUTED).width(96),
            seek,
            small("Next chapter", self.next().map(Msg::Chapter)),
        ]
        .spacing(14)
        .padding([0, 12])
        .height(BOTTOM_H)
        .align_y(Alignment::Center)
        .into()
    }

    fn thumbnails(&self) -> Element<'_, Msg> {
        let shown = match self.mode() {
            Mode::Pages => self.spread_of(self.page),
            Mode::Strip => {
                let p = self.current.unwrap_or(0);
                p..p + 1
            }
        };
        let mut list = Column::new()
            .spacing(10)
            .padding([12, 14])
            .align_x(Alignment::Center);
        for (i, slot) in self.slots.iter().enumerate() {
            let height = THUMB_W * self.strip.height(i) / self.strip.width();
            let picture: Element<'_, Msg> = match &slot.thumb {
                Some(handle) => iced::widget::image(handle.clone())
                    .width(THUMB_W)
                    .height(height)
                    .content_fit(ContentFit::Contain)
                    .into(),
                None => container(space())
                    .width(THUMB_W)
                    .height(height)
                    .style(theme::fill(theme::SURFACE, 2.0))
                    .into(),
            };
            let on = shown.contains(&i);
            list = list.push(
                button(
                    column![picture, text(format!("{}", i + 1)).size(11)]
                        .spacing(4)
                        .align_x(Alignment::Center),
                )
                .on_press(Msg::Seek(i as u16))
                .padding(4)
                .style(theme::thumb(on, self.accent)),
            );
        }
        container(scrollable(list).style(theme::scroll).height(Length::Fill))
            .width(THUMB_W + 40.0)
            .height(Length::Fill)
            .style(theme::fill(theme::alpha(theme::SURFACE, 0.35), 0.0))
            .into()
    }

    fn bar(&self) -> Element<'_, Msg> {
        let chapter = pick_list(
            self.choices.as_slice(),
            self.choices.get(self.index).cloned(),
            Msg::PickChapter,
        )
        .text_size(13)
        .padding([4, 10])
        .style(theme::picker)
        .menu_style(theme::menu);
        let mode = |m: Mode, label| {
            button(text(label).size(12))
                .on_press(Msg::SetMode(m))
                .padding([4, 10])
                .style(theme::chip(self.mode() == m))
        };
        let mut controls = row![mode(Mode::Strip, "Scroll"), mode(Mode::Pages, "Pages")]
            .spacing(4)
            .align_y(Alignment::Center);
        let toggle = |label, on: bool, msg: fn(bool) -> Msg| {
            button(text(label).size(12))
                .on_press(msg(!on))
                .padding([4, 10])
                .style(theme::chip(on))
        };
        match self.mode() {
            Mode::Pages => {
                controls = controls
                    .push(space().width(8))
                    .push(toggle("Two pages", self.settings.two_pages, Msg::TwoPages))
                    .push(toggle(
                        "Right to left",
                        self.right_to_left(),
                        Msg::RightToLeft,
                    ));
            }
            Mode::Strip => {
                controls = controls
                    .push(space().width(8))
                    .push(small("Smaller", Some(Msg::Zoom(1.0 / 1.15))))
                    .push(small("Larger", Some(Msg::Zoom(1.15))));
            }
        }
        row![
            small("Back", Some(Msg::Close)),
            text(&self.series.title)
                .font(theme::DISPLAY)
                .size(16)
                .wrapping(text::Wrapping::None),
            chapter,
            space().width(Length::Fill),
            controls,
            toggle("Thumbnails", self.settings.thumbnails, Msg::Thumbnails),
            small("Hide bars", Some(Msg::ToggleBars)),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding([0, 12])
        .height(BAR_H - 2.0)
        .into()
    }

    fn progress_line(&self) -> Element<'_, Msg> {
        let total = self.slots.len().max(1) as u16;
        let done = match self.current {
            Some(p) if !self.slots.is_empty() => (p + 1) as u16,
            _ => 0,
        };
        row![
            container(space())
                .width(Length::FillPortion(done))
                .height(2)
                .style(theme::line(self.accent)),
            container(space())
                .width(Length::FillPortion(total - done.min(total)))
                .height(2)
                .style(theme::line(theme::SURFACE)),
        ]
        .into()
    }

    fn strip_view(&self) -> Element<'_, Msg> {
        let window = self.strip.window(self.offset, self.viewport.1);
        let width = self.strip.width();
        let mut pages = Column::new()
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .push(space().height(self.strip.top(window.start)));
        for i in window.clone() {
            pages = pages.push(self.page_element(
                i,
                Length::Fixed(width),
                Length::Fixed(self.strip.height(i)),
            ));
        }
        let below = self.strip.top(self.strip.len()) - self.strip.top(window.end);
        pages = pages.push(space().height(below)).push(self.footer());
        scrollable(pages)
            .id(STRIP_ID)
            .on_scroll(Msg::Scrolled)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::scroll)
            .into()
    }

    fn page_view(&self) -> Element<'_, Msg> {
        let spread = self.spread_of(self.page);
        let pages: Element<'_, Msg> = if spread.len() == 2 {
            // Facing pages meet in the middle; right to left puts the first on the right.
            let (left, right) = if self.right_to_left() {
                (spread.start + 1, spread.start)
            } else {
                (spread.start, spread.start + 1)
            };
            let side = |i, align| {
                container(self.page_element(i, Length::Fill, Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(align)
            };
            row![side(left, Alignment::End), side(right, Alignment::Start)].into()
        } else {
            self.page_element(spread.start, Length::Fill, Length::Fill)
        };
        let page = container(pages)
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill);
        // The outer thirds turn towards their side; the middle shows or hides the bars.
        let (left, right) = if self.right_to_left() {
            (1, -1)
        } else {
            (-1, 1)
        };
        let zone = |msg, portion| {
            mouse_area(
                space()
                    .width(Length::FillPortion(portion))
                    .height(Length::Fill),
            )
            .on_press(msg)
            .interaction(mouse::Interaction::Pointer)
        };
        stack![
            page,
            row![
                zone(Msg::Turn(left), 2),
                zone(Msg::ToggleBars, 1),
                zone(Msg::Turn(right), 2)
            ]
        ]
        .into()
    }

    fn page_element(&self, i: usize, width: Length, height: Length) -> Element<'_, Msg> {
        let slot = &self.slots[i];
        match (&slot.image, &slot.error) {
            (Some(handle), _) => iced::widget::image(handle.clone())
                .width(width)
                .height(height)
                .content_fit(ContentFit::Contain)
                .filter_method(FilterMethod::Linear)
                .into(),
            (None, Some(e)) => {
                placeholder(format!("Page {} didn't load: {e}", i + 1), width, height)
            }
            (None, None) => placeholder(format!("{}", i + 1), width, height),
        }
    }

    fn footer(&self) -> Element<'_, Msg> {
        let end = text(format!("End of chapter {}", self.chapter().number))
            .font(theme::DISPLAY)
            .size(22);
        let next: Element<'_, Msg> = match self.next() {
            Some(i) => button(text(format!("Read chapter {}", self.chapters[i].number)))
                .on_press(Msg::Chapter(i))
                .padding([9, 20])
                .style(theme::paper)
                .into(),
            None => text(format!(
                "That's the newest chapter on {}.",
                self.source_id.name()
            ))
            .size(13)
            .color(theme::MUTED)
            .into(),
        };
        container(column![end, next].spacing(14).align_x(Alignment::Center))
            .center_x(Length::Fill)
            .height(crate::strip::FOOTER_H)
            .padding(padding::top(36))
            .into()
    }
}

fn chapter_label(c: &ChapterRef) -> String {
    match &c.title {
        Some(title) if !title.trim().is_empty() => {
            format!("Chapter {}: {}", c.number, title.trim())
        }
        _ => format!("Chapter {}", c.number),
    }
}

fn small(label: &str, msg: Option<Msg>) -> Element<'_, Msg> {
    button(text(label).size(12))
        .on_press_maybe(msg)
        .padding([4, 10])
        .style(theme::ghost)
        .into()
}

fn placeholder<'a>(label: String, width: Length, height: Length) -> Element<'a, Msg> {
    container(text(label).font(theme::SERIF).size(18).color(theme::FAINT))
        .center(Length::Fill)
        .width(width)
        .height(height)
        .style(theme::fill(theme::alpha(theme::SURFACE, 0.5), 2.0))
        .into()
}

async fn fetch_page(source: &dyn Source, page: &PageRef) -> Result<Fetched, String> {
    let bytes = source
        .fetch_page(page)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let size = page_size(&bytes)?;
    Ok((Arc::new(bytes), size))
}

/// Decodes a page, scaled down to `max_width` when it's wider.
fn decode(bytes: &[u8], max_width: u32) -> anyhow::Result<image::Handle> {
    let img = ::image::load_from_memory(bytes)?;
    let img = if img.width() > max_width {
        img.resize(max_width, u32::MAX, ::image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(image::Handle::from_rgba(w, h, rgba.into_raw()))
}

/// A page's size from its header, without decoding it.
fn page_size(bytes: &[u8]) -> Result<(u32, u32), String> {
    ::image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_dimensions()
        .map_err(|e| e.to_string())
}
