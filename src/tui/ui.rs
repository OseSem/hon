use std::sync::{Arc, mpsc};

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{FontSize, Resize, ResizeEncodeRender};
use tokio::sync::mpsc::UnboundedSender;

use super::app::{App, Fit, PageImage, Screen, is_strip};
use crate::commands::{gaps_text, source_name_width, updated_text};
use crate::config::Reader;
use crate::db::Progress;
use crate::model::{ChapterNo, ChapterRef, SourceId};

const SEARCH_HINTS: &str = "↑↓ move  ⇥ pick source  enter open  / new search  q quit";
const EDIT_HINTS: &str = "enter search  esc cancel";
const CHAPTER_HINTS: &str =
    "enter read  o open in viewer  d download  m mark read  s switch source  esc back";
const VIEWER_CHAPTER_HINTS: &str =
    "enter open  o open in viewer  d download  m mark read  s switch source  esc back";
const READER_HINTS: &str = "←/→ page  f fit  o viewer  q back";
/// Spaces between the reader's status and its key hints.
const HINTS_GAP: usize = 6;
/// Fewest characters of the title kept to make room for the reader's key hints.
const MIN_TITLE: usize = 4;
/// Pages are scaled up or down to fill the area, smoothly.
const RESIZE: Resize = Resize::Scale(Some(FilterType::Triangle));
/// Width `Ch N  <title>` is padded or cut to, before the dim annotations.
const LABEL_WIDTH: usize = 33;
/// Rows kept visible above and below the chapter cursor.
const CONTEXT_ROWS: usize = 2;

pub fn draw(f: &mut Frame, app: &mut App, view: &mut PageView) {
    match app.screen {
        Screen::Reader => return draw_reader(f, app, view),
        Screen::EndOfChapter => return draw_modal(f, end_lines(app)),
        Screen::SwitchSource => return draw_modal(f, switch_lines(app)),
        Screen::Search | Screen::Chapters => {}
    }
    let [header, _, body, status, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let hint = match app.screen {
        Screen::Search => {
            f.render_widget(search_header(app), header);
            draw_results(f, body, app);
            if app.editing {
                EDIT_HINTS
            } else {
                SEARCH_HINTS
            }
        }
        _ => {
            f.render_widget(chapters_header(app), header);
            draw_chapters(f, body, app);
            match app.reader_mode {
                Reader::Terminal => CHAPTER_HINTS,
                Reader::Viewer => VIEWER_CHAPTER_HINTS,
            }
        }
    };
    let status_line = match (&app.status, app.searching) {
        (Some(msg), _) => Line::raw(msg.as_str()),
        (None, true) => Line::raw("searching…").dim(),
        (None, false) => Line::default(),
    };
    f.render_widget(status_line, status);
    f.render_widget(Line::raw(hint).dim(), hints);
}

fn search_header(app: &App) -> Line<'_> {
    let mut spans = vec![
        "hon".fg(Color::Blue),
        " ".into(),
        "›".dim(),
        " search: ".into(),
        app.query.as_str().into(),
    ];
    if app.editing {
        spans.push("▏".fg(Color::Blue));
    }
    Line::from(spans)
}

fn draw_results(f: &mut Frame, area: Rect, app: &mut App) {
    let now = jiff::Timestamp::now();
    let name_width = source_name_width();
    let mut lines = Vec::new();
    let (mut first, mut last) = (0, 0);
    for (gi, group) in app.groups.iter().enumerate() {
        let selected = gi == app.selected;
        if selected {
            first = lines.len();
        }
        let marker = if selected { " ▸ " } else { "   " };
        lines.push(Line::raw(format!("{marker}{}", group.title)));
        for (si, s) in group.sources.iter().enumerate() {
            let picked = selected && si == app.picked_source;
            let emphasis = if picked {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let latest = s.latest.map_or("–".to_string(), |n| n.to_string());
            let mut spans = vec![
                "     ".into(),
                Span::styled(
                    format!("{:<name_width$}", s.manga.source.name()),
                    emphasis.fg(Color::Green),
                ),
                format!("   {:>3} ch   latest ", s.chapters.len()).into(),
                Span::styled(latest, emphasis),
            ];
            if let Some(updated) = s.updated {
                spans.push("   ".into());
                spans.push(updated_text(updated, now).dim());
            }
            if !s.gaps.is_empty() {
                spans.push("   ".into());
                spans.push(format!("gaps: {}", gaps_text(&s.gaps)).dim());
            }
            lines.push(Line::from(spans));
        }
        if selected {
            last = lines.len() - 1;
        }
    }
    app.search_scroll = scroll(app.search_scroll, first, last, lines.len(), area.height);
    let highlight = (!app.groups.is_empty()).then_some(first);
    draw_lines(f, area, lines, app.search_scroll, highlight);
}

fn chapters_header(app: &App) -> Line<'_> {
    let Some((group, source)) = &app.chosen else {
        return Line::default();
    };
    let chapters = app.chapters();
    let read = chapters.iter().filter(|c| app.is_read(c.number)).count();
    Line::from(vec![
        group.title.as_str().bold(),
        "  ".into(),
        "via".dim(),
        " ".into(),
        source.name().fg(Color::Blue),
        "  ".into(),
        format!("· {read}/{} read", chapters.len()).dim(),
    ])
}

fn draw_chapters(f: &mut Frame, area: Rect, app: &mut App) {
    let Some((_, source)) = &app.chosen else {
        return;
    };
    let chapters = app.chapters();
    if chapters.is_empty() {
        f.render_widget(
            Line::raw(format!("   no chapters on {}", source.name())).dim(),
            area,
        );
        return;
    }
    let lines: Vec<Line> = chapters
        .iter()
        .enumerate()
        .map(|(i, ch)| chapter_line(app, *source, ch, i == app.cursor))
        .collect();
    let context = CONTEXT_ROWS.min(usize::from(area.height.saturating_sub(1)) / 2);
    let first = app.cursor.saturating_sub(context);
    let last = (app.cursor + context).min(lines.len() - 1);
    let offset = scroll(app.chapter_scroll, first, last, lines.len(), area.height);
    let cursor = app.cursor;
    app.chapter_scroll = offset;
    draw_lines(f, area, lines, offset, Some(cursor));
}

fn chapter_line<'a>(app: &App, source: SourceId, ch: &ChapterRef, selected: bool) -> Line<'a> {
    let n = ch.number;
    let progress = app.progress.get(&n);
    let mark = match progress {
        Some(p) if p.read => "✓ ".fg(Color::Green),
        Some(Progress {
            last_page: Some(_), ..
        }) => "◐ ".fg(Color::Yellow),
        _ => "  ".into(),
    };
    let mut label = format!("Ch {n}");
    if let Some(title) = &ch.title {
        label += "  ";
        label += title;
    }
    let mut notes = Vec::new();
    if let Some(Progress {
        read: false,
        last_page: Some(page),
        page_count,
        source: Some(on),
        ..
    }) = progress
        && *on == source
    {
        let of = page_count.map_or(String::new(), |c| format!("/{c}"));
        notes.push(format!("p {}{of} · resume", page + 1));
    }
    if app.downloaded.contains(&n) {
        notes.push("↓ downloaded".to_string());
    } else if app.downloading.contains_key(&n) {
        notes.push("↓ downloading…".to_string());
    }
    let mut spans = vec![(if selected { " ▸ " } else { "   " }).into(), mark];
    if notes.is_empty() {
        spans.push(label.into());
    } else {
        spans.push(format!("{:<LABEL_WIDTH$} ", truncate(&label, LABEL_WIDTH)).into());
        spans.push(notes.join("  ").dim());
    }
    Line::from(spans)
}

fn draw_reader(f: &mut Frame, app: &mut App, view: &mut PageView) {
    let status_rows = u16::from(app.status.is_some());
    let [body, status, bar] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(status_rows),
        Constraint::Length(1),
    ])
    .areas(f.area());
    if let Some(msg) = &app.status {
        f.render_widget(Line::raw(msg.as_str()), status);
    }
    f.render_widget(reader_bar(app, bar.width), bar);

    let chosen = app.fit;
    let Some(reader) = &mut app.reader else {
        return;
    };
    let fit = reader.fit(chosen);
    reader.max_scroll = 0;
    reader.view_rows = body.height;
    let n = reader.ch.number;
    let shown = reader.page + 1;
    let message = match &reader.pages {
        None => Line::raw(format!("loading Ch {n}…")).dim(),
        Some(Err(e)) => Line::raw(format!("couldn't load Ch {n}: {e} (r to retry)")),
        Some(Ok(_)) => match reader.images.get(&reader.page) {
            Some(PageImage::Ready(image)) => {
                let page = PageId {
                    source: reader.ch.source,
                    ch: n,
                    page: reader.page,
                };
                reader.max_scroll = view.draw(f, body, page, image, fit, &mut reader.scroll);
                return;
            }
            Some(PageImage::Failed(e)) => {
                Line::raw(format!("couldn't load page {shown}: {e} (r to retry)"))
            }
            Some(PageImage::Loading) | None => Line::raw(format!("loading page {shown}…")).dim(),
        },
    };
    draw_message(f, body, message);
}

/// One line, centered in `area`.
fn draw_message(f: &mut Frame, area: Rect, line: Line) {
    let row = area.centered_vertically(Constraint::Length(1));
    f.render_widget(line.centered(), row);
}

/// `Title · Ch N · p X/Y · Source` and the key hints, shortening the title to keep the
/// hints, and dropping the hints when even that isn't enough.
fn reader_bar(app: &App, width: u16) -> Line<'_> {
    let (Some((group, _)), Some(reader)) = (&app.chosen, &app.reader) else {
        return Line::default();
    };
    let page = match &reader.pages {
        Some(Ok(pages)) => format!("p {}/{}", reader.page + 1, pages.len()),
        _ => "p –".to_string(),
    };
    let rest = [
        Span::raw(format!(" · Ch {} · ", reader.ch.number)),
        page.fg(Color::Blue),
        Span::raw(format!(" · {}", reader.ch.source.name())),
    ];
    let rest_width: usize = rest.iter().map(Span::width).sum();
    let with_hints = rest_width + HINTS_GAP + READER_HINTS.chars().count();
    let width = usize::from(width);
    let title_width = group.title.chars().count();
    let (title, hints) = if title_width + with_hints <= width {
        (group.title.clone(), true)
    } else if width >= with_hints + MIN_TITLE {
        (truncate(&group.title, width - with_hints), true)
    } else {
        (group.title.clone(), false)
    };
    let mut spans = vec![Span::raw(title)];
    spans.extend(rest);
    if hints {
        spans.push(Span::raw(" ".repeat(HINTS_GAP)));
        spans.push(Span::raw(READER_HINTS));
    }
    Line::from(spans).dim()
}

fn end_lines(app: &App) -> Vec<Line<'_>> {
    let Some(reader) = &app.reader else {
        return Vec::new();
    };
    let next = match app.next_chapter() {
        Some(ch) => {
            let mut label = format!("  Next: Ch {}", ch.number);
            if let Some(title) = &ch.title {
                label += " · ";
                label += title;
            }
            Line::from(vec!["enter".fg(Color::Blue), label.into()])
        }
        None => Line::from(vec!["enter".dim(), "  No next chapter yet".dim()]),
    };
    vec![
        Line::from(vec![
            format!("Finished Ch {}", reader.ch.number).bold(),
            "  ".into(),
            "✓ marked read".fg(Color::Green),
        ]),
        Line::default(),
        next,
        Line::from(vec![
            "l".fg(Color::Blue),
            "      Back to chapter list".into(),
        ]),
        Line::from(vec!["q".fg(Color::Blue), "      Quit".into()]),
    ]
}

fn switch_lines(app: &App) -> Vec<Line<'_>> {
    let (Some((_, on)), Some(ch)) = (&app.chosen, &app.switch_to) else {
        return Vec::new();
    };
    vec![Line::raw(format!(
        "Ch {} isn't on {} — read it from {}? [y/n]",
        ch.number,
        on.name(),
        ch.source.name()
    ))]
}

/// `lines` in a rounded box in the middle of the screen.
fn draw_modal(f: &mut Frame, lines: Vec<Line>) {
    let width = lines.iter().map(Line::width).max().unwrap_or(0) + 4;
    let height = lines.len() + 2;
    let area = f.area().centered(
        Constraint::Length(u16::try_from(width).unwrap_or(u16::MAX)),
        Constraint::Length(u16::try_from(height).unwrap_or(u16::MAX)),
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Color::Blue)
        .padding(Padding::horizontal(1));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageId {
    pub source: SourceId,
    pub ch: ChapterNo,
    pub page: usize,
}

/// What the reader shows: a page, fitted one way and scrolled, in an area of the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewKey {
    page: PageId,
    fit: Fit,
    scroll: u16,
    area: Size,
}

/// A page scaled and encoded for the terminal by the view's worker thread.
pub struct Encoded {
    key: ViewKey,
    protocol: Result<StatefulProtocol, String>,
}

struct Job {
    key: ViewKey,
    image: Arc<DynamicImage>,
    /// Source rows `(top, height)` to show, when fitting to width.
    rows: Option<(u32, u32)>,
}

/// Draws the current page. Scaling and encoding happen on a worker thread, so the event
/// loop never waits on them: a page appears once its `Encoded` comes back through
/// `finished`, and a resize asks for a new encoding.
pub struct PageView {
    picker: Picker,
    jobs: mpsc::Sender<Job>,
    requested: Option<ViewKey>,
    shown: Option<Encoded>,
}

impl PageView {
    pub fn new(picker: Picker, done: UnboundedSender<Encoded>) -> PageView {
        let (jobs, queue) = mpsc::channel::<Job>();
        let worker_picker = picker.clone();
        // Ends when the view (the only sender) is dropped.
        std::thread::spawn(move || {
            while let Ok(mut job) = queue.recv() {
                // Only the newest request matters.
                while let Ok(newer) = queue.try_recv() {
                    job = newer;
                }
                if done.send(encode(&worker_picker, job)).is_err() {
                    break;
                }
            }
        });
        PageView {
            picker,
            jobs,
            requested: None,
            shown: None,
        }
    }

    pub fn finished(&mut self, encoded: Encoded) {
        if Some(encoded.key) == self.requested {
            self.shown = Some(encoded);
        }
    }

    fn draw(
        &mut self,
        f: &mut Frame,
        area: Rect,
        page: PageId,
        image: &Arc<DynamicImage>,
        fit: Fit,
        scroll: &mut u16,
    ) -> u16 {
        if area.is_empty() {
            return 0;
        }
        let font = self.picker.font_size();
        let (area, rows, max_scroll) = match fit {
            Fit::Height => {
                *scroll = 0;
                (area, None, 0)
            }
            Fit::Width => {
                let area = if is_strip(image) {
                    strip_area(area, font)
                } else {
                    area
                };
                let (rows, max) = crop_rows(image.width(), image.height(), area, font, scroll);
                (area, Some(rows), max)
            }
        };
        let key = ViewKey {
            page,
            fit,
            scroll: *scroll,
            area: area.as_size(),
        };
        if self.requested != Some(key) {
            self.requested = Some(key);
            let job = Job {
                key,
                image: Arc::clone(image),
                rows,
            };
            // The worker only stops once this view is dropped.
            let _ = self.jobs.send(job);
        }
        match &mut self.shown {
            Some(Encoded {
                key: shown,
                protocol: Ok(protocol),
            }) if *shown == key => {
                let size = protocol.size_for(RESIZE, area.as_size());
                let rect = area.centered(
                    Constraint::Length(size.width),
                    Constraint::Length(size.height),
                );
                protocol.render(rect, f.buffer_mut());
            }
            Some(Encoded {
                key: shown,
                protocol: Err(e),
            }) if *shown == key => {
                let line = Line::raw(format!("couldn't draw page {}: {e}", page.page + 1));
                draw_message(f, area, line);
            }
            _ => {
                let line = Line::raw(format!("loading page {}…", page.page + 1)).dim();
                draw_message(f, area, line);
            }
        }
        max_scroll
    }
}

/// The middle of `area` a long strip is fitted to the width of: no wider than three
/// quarters of its height, so a wide screen shows more of the strip than a few panels.
fn strip_area(area: Rect, font: FontSize) -> Rect {
    let max_px = u32::from(area.height) * u32::from(font.height) * 3 / 4;
    let columns = max_px / u32::from(font.width.max(1));
    let columns = u16::try_from(columns)
        .unwrap_or(u16::MAX)
        .clamp(1, area.width);
    area.centered_horizontally(Constraint::Length(columns))
}

fn encode(picker: &Picker, job: Job) -> Encoded {
    let image = match job.rows {
        Some((top, height)) => job.image.crop_imm(0, top, job.image.width(), height),
        None => DynamicImage::clone(&job.image),
    };
    let mut protocol = picker.new_resize_protocol(image);
    let size = protocol.size_for(RESIZE, job.key.area);
    protocol.resize_encode(&RESIZE, size);
    let protocol = match protocol.last_encoding_result() {
        Some(Err(e)) => Err(e.to_string()),
        _ => Ok(protocol),
    };
    Encoded {
        key: job.key,
        protocol,
    }
}

/// For a `width`×`height` px page fitted to the width of `area`: the source rows
/// `(top, height)` that fill the area when scrolled down `scroll` rows. Clamps `scroll`
/// to the bottom of the page, and returns that bottom too.
fn crop_rows(
    width: u32,
    height: u32,
    area: Rect,
    font: FontSize,
    scroll: &mut u16,
) -> ((u32, u32), u16) {
    let (width, height) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let row_px = f64::from(font.height);
    // Screen pixels per page pixel.
    let scale = f64::from(area.width) * f64::from(font.width) / width;
    let visible = (f64::from(area.height) * row_px / scale).min(height);
    let max_scroll = ((height - visible) * scale / row_px).ceil() as u16;
    *scroll = (*scroll).min(max_scroll);
    let top = (f64::from(*scroll) * row_px / scale).min(height - visible);
    ((top as u32, (visible as u32).max(1)), max_scroll)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Renders `lines[offset..]` into `area`, with the `highlight` line in the accent colour.
fn draw_lines(
    f: &mut Frame,
    area: Rect,
    lines: Vec<Line>,
    offset: usize,
    highlight: Option<usize>,
) {
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(offset)
        .take(area.height.into())
        .collect();
    f.render_widget(Paragraph::new(visible), area);
    if let Some(row) = highlight
        .and_then(|i| i.checked_sub(offset))
        .and_then(|i| u16::try_from(i).ok())
        .filter(|&i| i < area.height)
    {
        let row = Rect {
            y: area.y + row,
            height: 1,
            ..area
        };
        f.buffer_mut().set_style(row, Style::new().bg(Color::Blue));
    }
}

/// Smallest change to `offset` that keeps lines `first..=last` (at least `first`)
/// within `height` rows, without scrolling past the end.
fn scroll(offset: usize, first: usize, last: usize, len: usize, height: u16) -> usize {
    let height = usize::from(height);
    offset
        .max((last + 1).saturating_sub(height))
        .min(first)
        .min(len.saturating_sub(height))
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use std::sync::Arc;

    use ratatui::buffer::Buffer;
    use ratatui_image::FontSize;
    use ratatui_image::picker::Picker;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::db::Progress;
    use crate::model::SourceId;
    use crate::tui::app::AppEvent;
    use crate::tui::app::test_data::*;

    fn view() -> (PageView, UnboundedReceiver<Encoded>) {
        let (tx, rx) = unbounded_channel();
        (PageView::new(Picker::halfblocks(), tx), rx)
    }

    fn draw_buffer(app: &mut App, view: &mut PageView, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app, view)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn lines(buf: &Buffer) -> Vec<String> {
        let area = buf.area;
        (0..area.height)
            .map(|y| {
                let row: String = (0..area.width).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect()
    }

    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        lines(&draw_buffer(app, &mut view().0, width, height))
    }

    fn assert_contains(screen: &[String], text: &str) {
        assert!(
            screen.iter().any(|l| l.contains(text)),
            "missing {text:?} in:\n{}",
            screen.join("\n")
        );
    }

    /// Columns of row `y` an image was drawn into.
    fn image_columns(buf: &Buffer, y: u16) -> Vec<u16> {
        (0..buf.area.width)
            .filter(|&x| buf[(x, y)].bg != Color::Reset)
            .collect()
    }

    fn encoded(rx: &mut UnboundedReceiver<Encoded>) -> Encoded {
        for _ in 0..500 {
            if let Ok(done) = rx.try_recv() {
                return done;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("page wasn't encoded");
    }

    fn assert_has(screen: &[String], line: &str) {
        assert!(
            screen.iter().any(|l| l == line),
            "missing line {line:?} in:\n{}",
            screen.join("\n")
        );
    }

    #[test]
    fn search_screen_renders_mockup_lines() {
        let mut app = searched();
        let screen = render(&mut app, 80, 24);
        assert_eq!(screen[0], "hon › search: solo leveling");
        for line in [
            " ▸ Solo Leveling",
            "     MangaKatana   201 ch   latest 201   updated 2d ago",
            "     MangaDex      179 ch   latest 200   gaps: 45–66",
            "   Solo Leveling: Ragnarok",
            "     MangaKatana    58 ch   latest 58",
            "     MangaDex       54 ch   latest 54",
            "   Solo Leveling: Side Story",
            "     MangaDex       21 ch   latest 21",
        ] {
            assert_has(&screen, line);
        }
        assert_eq!(
            screen[23],
            "↑↓ move  ⇥ pick source  enter open  / new search  q quit"
        );
    }

    #[test]
    fn search_screen_shows_cursor_and_searching() {
        let (mut app, _) = App::new(crate::tui::Start::Search, Reader::Terminal);
        typed(&mut app, "solo");
        let screen = render(&mut app, 80, 24);
        assert_eq!(screen[0], "hon › search: solo▏");
        app.update(key(KeyCode::Enter));
        assert_has(&render(&mut app, 80, 24), "searching…");
    }

    #[test]
    fn chapter_screen_renders_marks() {
        let mut progress = read((1..=111).chain([150]));
        progress.insert(
            n(112),
            Progress {
                read: false,
                last_page: Some(13),
                page_count: Some(38),
                source: Some(SourceId::MangaKatana),
                updated_at: 1,
            },
        );
        let mut app = chapters(progress);
        app.update(AppEvent::Downloaded(n(114), Ok("/x".into())));
        let screen = render(&mut app, 80, 24);
        assert_eq!(screen[0], "Solo Leveling  via MangaKatana  · 112/201 read");
        assert_eq!(screen[2], "   ✓ Ch 110  The Hunter Association");
        for line in [
            "   ✓ Ch 111  Raid",
            " ▸ ◐ Ch 112  Shadow Monarch            p 14/38 · resume",
            "     Ch 113  Return",
            "     Ch 114  Jeju Island               ↓ downloaded",
            "     Ch 115  Ants",
        ] {
            assert_has(&screen, line);
        }
        assert_eq!(
            screen[23],
            "enter read  o open in viewer  d download  m mark read  s switch source  esc back"
        );

        // Progress from another source: mark only, no resume page.
        app.update(key(KeyCode::Char('s')));
        assert_has(&render(&mut app, 80, 24), " ▸ ◐ Ch 112");
    }

    #[test]
    fn chapter_hints_say_enter_opens_in_viewer_mode() {
        let mut app = chapters_in_viewer(read([]));
        assert_eq!(
            render(&mut app, 80, 24)[23],
            "enter open  o open in viewer  d download  m mark read  s switch source  esc back"
        );
    }

    #[test]
    fn renders_in_tiny_terminal() {
        let mut app = searched();
        render(&mut app, 20, 5);
        let mut app = chapters(read(1..=50));
        render(&mut app, 20, 5);
        for (w, h) in [(1, 1), (0, 0), (80, 2)] {
            render(&mut app, w, h);
        }

        let (mut view, _rx) = view();
        reading(&mut app, 51, 2);
        app.update(AppEvent::PageLoaded(0, Ok(page_image())));
        app.update(key(KeyCode::Char('f')));
        for (w, h) in [(20, 5), (1, 1), (0, 0), (80, 1), (80, 2)] {
            draw_buffer(&mut app, &mut view, w, h);
        }
        // The page is taller than the screen, so → scrolls through it before turning.
        for _ in 0..100 {
            if app.screen == Screen::EndOfChapter {
                break;
            }
            app.update(key(KeyCode::Right));
        }
        assert_eq!(app.screen, Screen::EndOfChapter);
        for (w, h) in [(20, 5), (1, 1), (0, 0)] {
            render(&mut app, w, h);
        }
    }

    #[test]
    fn reader_status_bar_renders() {
        let mut progress = read(1..=111);
        progress.insert(n(112), in_progress(13, 38, SourceId::MangaKatana));
        let mut app = chapters(progress);
        reading(&mut app, 112, 38);
        let screen = render(&mut app, 100, 30);
        assert_eq!(
            screen[29],
            "Solo Leveling · Ch 112 · p 14/38 · MangaKatana      ←/→ page  f fit  o viewer  q back"
        );
        assert_contains(&screen, "loading page 14…");

        assert_eq!(
            render(&mut app, 80, 30)[29],
            "Solo Le… · Ch 112 · p 14/38 · MangaKatana      ←/→ page  f fit  o viewer  q back",
            "the title gives way to the key hints"
        );
        assert_eq!(
            render(&mut app, 50, 30)[29],
            "Solo Leveling · Ch 112 · p 14/38 · MangaKatana",
            "too narrow for hints"
        );

        app.status = Some("opening Ch 112…".into());
        assert_eq!(render(&mut app, 100, 30)[28], "opening Ch 112…");
    }

    #[test]
    fn page_error_shows_retry() {
        let mut app = chapters(read([]));
        reading(&mut app, 3, 10);
        app.update(AppEvent::PageLoaded(
            0,
            Err(anyhow::anyhow!("HTTP 404 Not Found")),
        ));
        assert_contains(
            &render(&mut app, 80, 24),
            "couldn't load page 1: HTTP 404 Not Found (r to retry)",
        );

        app.update(key(KeyCode::Char('q')));
        app.update(key(KeyCode::Enter));
        assert_contains(&render(&mut app, 80, 24), "loading Ch 3…");
        app.update(AppEvent::PagesListed(
            n(3),
            Err(anyhow::anyhow!("timed out")),
        ));
        assert_contains(
            &render(&mut app, 80, 24),
            "couldn't load Ch 3: timed out (r to retry)",
        );
    }

    #[test]
    fn end_prompt_renders() {
        let mut app = chapters(read(1..=111));
        reading(&mut app, 112, 1);
        app.update(key(KeyCode::Right));
        let screen = render(&mut app, 80, 24);
        for text in [
            "Finished Ch 112  ✓ marked read",
            "enter  Next: Ch 113 · Return",
            "l      Back to chapter list",
            "q      Quit",
        ] {
            assert_contains(&screen, text);
        }

        let mut app = chapters(read([]));
        reading(&mut app, 201, 1);
        app.update(key(KeyCode::Right));
        assert_contains(&render(&mut app, 80, 24), "enter  No next chapter yet");
    }

    #[test]
    fn switch_source_prompt_renders() {
        let mut app = chapters_on_mangadex(read([]));
        reading(&mut app, 44, 1);
        app.update(key(KeyCode::Right));
        app.update(key(KeyCode::Enter));
        assert_contains(
            &render(&mut app, 80, 24),
            "Ch 45 isn't on MangaDex — read it from MangaKatana? [y/n]",
        );
    }

    #[test]
    fn page_is_encoded_off_the_draw_and_again_on_resize() {
        let (mut view, mut rx) = view();
        let mut app = chapters(read([]));
        reading(&mut app, 1, 3);
        let image =
            image::RgbImage::from_fn(40, 60, |x, y| image::Rgb([x as u8 * 6, y as u8 * 4, 90]));
        app.update(AppEvent::PageLoaded(0, Ok(Arc::new(image.into()))));

        let buf = draw_buffer(&mut app, &mut view, 40, 20);
        assert!(
            image_columns(&buf, 0).is_empty(),
            "drawn before it's encoded"
        );
        view.finished(encoded(&mut rx));
        let buf = draw_buffer(&mut app, &mut view, 40, 20);
        // 40×60 px scaled into 40×19 cells of 10×20 px: 26 columns wide, centered.
        assert_eq!(image_columns(&buf, 0), (7..33).collect::<Vec<_>>());
        assert_eq!(image_columns(&buf, 18).len(), 26);

        let buf = draw_buffer(&mut app, &mut view, 60, 30);
        assert!(
            image_columns(&buf, 0).is_empty(),
            "the old size isn't drawn after a resize"
        );
        view.finished(encoded(&mut rx));
        let buf = draw_buffer(&mut app, &mut view, 60, 30);
        assert_eq!(image_columns(&buf, 0).len(), 39);

        app.update(key(KeyCode::Char('f')));
        draw_buffer(&mut app, &mut view, 60, 30);
        view.finished(encoded(&mut rx));
        let buf = draw_buffer(&mut app, &mut view, 60, 30);
        assert_eq!(image_columns(&buf, 0).len(), 60, "fit to width");
    }

    #[test]
    fn page_shows_a_loading_line_while_it_is_encoded() {
        let (mut view, mut rx) = view();
        let mut app = chapters(read([]));
        reading(&mut app, 1, 3);
        let image = image::RgbImage::from_pixel(40, 60, image::Rgb([200, 100, 50]));
        app.update(AppEvent::PageLoaded(0, Ok(Arc::new(image.into()))));

        let screen = lines(&draw_buffer(&mut app, &mut view, 40, 20));
        assert_contains(&screen, "loading page 1…");

        view.finished(encoded(&mut rx));
        let screen = lines(&draw_buffer(&mut app, &mut view, 40, 20));
        assert!(
            !screen.iter().any(|l| l.contains("loading page")),
            "gone once it's drawn"
        );

        let screen = lines(&draw_buffer(&mut app, &mut view, 60, 30));
        assert_contains(&screen, "loading page 1…");
    }

    #[test]
    fn long_strip_fits_a_column_as_wide_as_three_quarters_of_the_height() {
        let (mut view, mut rx) = view();
        let mut app = chapters(read([]));
        reading(&mut app, 1, 3);
        let strip = image::RgbImage::from_pixel(40, 400, image::Rgb([200, 100, 50]));
        app.update(AppEvent::PageLoaded(0, Ok(Arc::new(strip.into()))));

        draw_buffer(&mut app, &mut view, 60, 30);
        view.finished(encoded(&mut rx));
        let buf = draw_buffer(&mut app, &mut view, 60, 30);
        // 29 rows of 20 px make 435 px, so 43 columns of 10 px, centered.
        assert_eq!(image_columns(&buf, 0), (9..52).collect::<Vec<_>>());
        assert_eq!(image_columns(&buf, 28).len(), 43, "fills the height");
        let reader = app.reader.as_ref().unwrap();
        assert!(reader.max_scroll > 0);
        assert_eq!(reader.view_rows, 29);
    }

    #[test]
    fn fit_width_crops_visible_rows() {
        let font = FontSize::new(10, 20);
        let area = Rect::new(0, 0, 10, 10);
        let mut scroll = 5;
        assert_eq!(
            crop_rows(100, 1000, area, font, &mut scroll),
            ((100, 200), 40)
        );
        scroll = 99;
        assert_eq!(
            crop_rows(100, 1000, area, font, &mut scroll),
            ((800, 200), 40)
        );
        assert_eq!(scroll, 40, "clamped to the bottom of the page");
        assert_eq!(crop_rows(100, 100, area, font, &mut scroll), ((0, 100), 0));
        assert_eq!(scroll, 0);
        let wide = Rect::new(0, 0, 20, 10);
        assert_eq!(
            crop_rows(100, 1000, wide, font, &mut scroll),
            ((0, 100), 90)
        );
    }
}
