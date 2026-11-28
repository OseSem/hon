//! The long-strip reader: lists a chapter's pages, fetches them in reading order, keeps only
//! the pages near the viewport decoded, and saves progress as the reader scrolls.

use std::io::Cursor;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use hon::commands::Ctx;
use hon_core::db::Series;
use hon_core::model::{ChapterRef, PageRef, SourceId};
use hon_core::sources::Source;
use iced::widget::image::{self, FilterMethod};
use iced::widget::operation::{self, AbsoluteOffset};
use iced::widget::{Column, button, center, column, container, row, scrollable, space, text};
use iced::{Alignment, Element, Length, Task, padding};
use tokio::runtime::Handle;

use crate::strip::Strip;

pub const STRIP_ID: &str = "reader-strip";
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
    Scrolled(scrollable::Viewport),
    Chapter(usize),
    Zoom(f32),
    Retry,
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
}

pub struct Reader {
    pub series: Series,
    pub source_id: SourceId,
    source: Arc<dyn Source>,
    /// The source's chapters, ascending.
    chapters: Vec<ChapterRef>,
    index: usize,
    /// Bumped on every chapter change so late results of the previous one are dropped.
    generation: u64,
    load: Load,
    strip: Strip,
    slots: Vec<Slot>,
    offset: f32,
    viewport: (f32, f32),
    zoom: f32,
    resume_page: u32,
    current: Option<usize>,
    marked_read: bool,
    /// Dropping it stops the running fetch.
    _fetch: Option<iced::task::Handle>,
}

impl Reader {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        rt: &Handle,
        series: Series,
        source: Arc<dyn Source>,
        chapters: Vec<ChapterRef>,
        index: usize,
        resume_page: u32,
        viewport: (f32, f32),
    ) -> (Reader, Task<Msg>) {
        let mut reader = Reader {
            series,
            source_id: source.id(),
            source,
            chapters,
            index,
            generation: 0,
            load: Load::Listing,
            strip: Strip::new(0, BASE_WIDTH),
            slots: Vec::new(),
            offset: 0.0,
            viewport,
            zoom: 1.0,
            resume_page,
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

    fn page_width(&self) -> f32 {
        (BASE_WIDTH * self.zoom)
            .min(self.viewport.0 - 24.0)
            .max(200.0)
    }

    fn list(&mut self, rt: &Handle) -> Task<Msg> {
        self.generation += 1;
        self.load = Load::Listing;
        self.slots.clear();
        self.strip = Strip::new(0, self.page_width());
        self.offset = 0.0;
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
        .chain(operation::scroll_to(
            STRIP_ID,
            AbsoluteOffset { x: 0.0, y: 0.0 },
        ))
    }

    pub fn update(&mut self, msg: Msg, ctx: &Ctx, rt: &Handle) -> (Task<Msg>, Option<String>) {
        match msg {
            Msg::Listed(g, _) | Msg::Fetched(g, ..) | Msg::Decoded(g, ..)
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
                self.offset = self.strip.top(start);
                let fetch = self.fetch(rt, pages, start);
                let scroll = operation::scroll_to(
                    STRIP_ID,
                    AbsoluteOffset {
                        x: 0.0,
                        y: self.offset,
                    },
                );
                (Task::batch([fetch, scroll]), None)
            }
            Msg::Fetched(_, i, Err(e)) => {
                self.slots[i].error = Some(e);
                (Task::none(), None)
            }
            Msg::Fetched(_, i, Ok((bytes, size))) => {
                self.slots[i].bytes = Some(bytes);
                let delta = self.strip.set_size(i, size, self.offset);
                let mut tasks = vec![self.decode_window(rt)];
                if delta != 0.0 {
                    self.offset += delta;
                    tasks.push(operation::scroll_to(
                        STRIP_ID,
                        AbsoluteOffset {
                            x: 0.0,
                            y: self.offset,
                        },
                    ));
                }
                (Task::batch(tasks), None)
            }
            Msg::Decoded(_, i, result) => {
                self.slots[i].decoding = false;
                let window = self.strip.window(self.offset, self.viewport.1);
                match result {
                    Ok(handle) if window.contains(&i) => self.slots[i].image = Some(handle),
                    Ok(_) => {}
                    Err(e) => self.slots[i].error = Some(e),
                }
                (Task::none(), None)
            }
            Msg::Scrolled(viewport) => {
                self.offset = viewport.absolute_offset().y;
                let bounds = viewport.bounds();
                let task = self.resize((bounds.width, bounds.height));
                let note = self.track(ctx).err().map(|e| format!("{e:#}"));
                (Task::batch([task, self.decode_window(rt)]), note)
            }
            Msg::Chapter(index) => {
                let note = self.mark_read(ctx).err().map(|e| format!("{e:#}"));
                self.index = index.min(self.chapters.len() - 1);
                (self.list(rt), note)
            }
            Msg::Zoom(factor) => {
                self.zoom = (self.zoom * factor).clamp(0.4, 3.0);
                let viewport = self.viewport;
                self.viewport = (0.0, 0.0);
                (
                    Task::batch([self.resize(viewport), self.decode_window(rt)]),
                    None,
                )
            }
            Msg::Retry => (self.list(rt), None),
        }
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
        operation::scroll_to(
            STRIP_ID,
            AbsoluteOffset {
                x: 0.0,
                y: self.offset,
            },
        )
    }

    pub fn scroll_by(&self, dy: f32) -> Task<Msg> {
        operation::scroll_by(STRIP_ID, AbsoluteOffset { x: 0.0, y: dy })
    }

    pub fn viewport_height(&self) -> f32 {
        self.viewport.1
    }

    pub fn next(&self) -> Option<usize> {
        (self.index + 1 < self.chapters.len()).then_some(self.index + 1)
    }

    pub fn previous(&self) -> Option<usize> {
        self.index.checked_sub(1)
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

    /// Decodes the fetched pages near the viewport and drops the decoded ones away from it.
    fn decode_window(&mut self, rt: &Handle) -> Task<Msg> {
        let window = self.strip.window(self.offset, self.viewport.1);
        let max_width = (self.strip.width() * MAX_SCALE) as u32;
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

    /// Saves the page being read, and marks the chapter read once its end is on screen.
    fn track(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        if self.slots.is_empty() {
            return Ok(());
        }
        let page = self.strip.current(self.offset, self.viewport.1);
        if self.current != Some(page) {
            self.current = Some(page);
            let total = self.slots.len() as u32;
            let number = self.chapter().number;
            ctx.db
                .save_page(self.series.id, number, &self.source_id, page as u32, total)?;
        }
        if self.strip.at_end(self.offset, self.viewport.1) {
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
        let ch = self.chapter();
        let mut heading = format!("{}  ·  Ch {}", self.series.title, ch.number);
        if let Some(title) = &ch.title {
            heading += &format!(" — {title}");
        }
        let page = match (&self.load, self.current) {
            (Load::Ready, Some(p)) => format!("{} / {}", p + 1, self.slots.len()),
            _ => String::new(),
        };
        let bar = row![
            text(heading).size(14).width(Length::Fill),
            text(page).size(13),
            button(text("−").size(14))
                .on_press(Msg::Zoom(1.0 / 1.15))
                .padding([2, 10]),
            button(text("+").size(14))
                .on_press(Msg::Zoom(1.15))
                .padding([2, 10]),
            nav_button("‹ Prev", self.previous()),
            nav_button("Next ›", self.next()),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding([6, 12]);

        let body: Element<'_, Msg> = match &self.load {
            Load::Listing => center(text("Loading pages…").size(14)).into(),
            Load::Failed(e) => center(
                column![
                    text(format!("Couldn't list the pages: {e}")).size(14),
                    button(text("Try again")).on_press(Msg::Retry),
                ]
                .spacing(12)
                .align_x(Alignment::Center),
            )
            .into(),
            Load::Ready => self.strip_view(),
        };
        column![bar, body].into()
    }

    fn strip_view(&self) -> Element<'_, Msg> {
        let window = self.strip.window(self.offset, self.viewport.1);
        let width = self.strip.width();
        let mut pages = Column::new()
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .push(space().height(self.strip.top(window.start)));
        for i in window.clone() {
            let height = self.strip.height(i);
            let slot = &self.slots[i];
            let page: Element<'_, Msg> = match (&slot.image, &slot.error) {
                (Some(handle), _) => iced::widget::image(handle.clone())
                    .width(width)
                    .height(height)
                    .filter_method(FilterMethod::Linear)
                    .into(),
                (None, Some(e)) => {
                    placeholder(format!("Page {} failed: {e}", i + 1), width, height)
                }
                (None, None) => placeholder(format!("Page {}", i + 1), width, height),
            };
            pages = pages.push(page);
        }
        let below = self.strip.top(self.strip.len()) - self.strip.top(window.end);
        pages = pages.push(space().height(below)).push(self.footer());
        scrollable(pages)
            .id(STRIP_ID)
            .on_scroll(Msg::Scrolled)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn footer(&self) -> Element<'_, Msg> {
        let end = text(format!("End of Ch {}", self.chapter().number)).size(14);
        let next: Element<'_, Msg> = match self.next() {
            Some(i) => button(text(format!("Next: Ch {}", self.chapters[i].number)))
                .on_press(Msg::Chapter(i))
                .padding([8, 16])
                .into(),
            None => text("No more chapters on this source yet").size(13).into(),
        };
        container(column![end, next].spacing(12).align_x(Alignment::Center))
            .center_x(Length::Fill)
            .height(crate::strip::FOOTER_H)
            .padding(padding::top(32))
            .into()
    }
}

fn nav_button(label: &str, chapter: Option<usize>) -> Element<'_, Msg> {
    button(text(label).size(13))
        .on_press_maybe(chapter.map(Msg::Chapter))
        .padding([2, 10])
        .into()
}

fn placeholder<'a>(label: String, width: f32, height: f32) -> Element<'a, Msg> {
    container(text(label).size(13))
        .center(Length::Fill)
        .width(width)
        .height(height)
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
