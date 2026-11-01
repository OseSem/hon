use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::app::{App, Screen};
use crate::commands::{gaps_text, source_name_width, updated_text};
use crate::db::Progress;
use crate::model::{ChapterRef, SourceId};

const SEARCH_HINTS: &str = "↑↓ move  ⇥ pick source  enter open  / new search  q quit";
const EDIT_HINTS: &str = "enter search  esc cancel";
const CHAPTER_HINTS: &str =
    "enter read  o open in viewer  d download  m mark read  s switch source  esc back";
/// Width `Ch N  <title>` is padded or cut to, before the dim annotations.
const LABEL_WIDTH: usize = 33;
/// Rows kept visible above and below the chapter cursor.
const CONTEXT_ROWS: usize = 2;

pub fn draw(f: &mut Frame, app: &mut App) {
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
        Screen::Chapters | Screen::Reader | Screen::EndOfChapter | Screen::SwitchSource => {
            f.render_widget(chapters_header(app), header);
            draw_chapters(f, body, app);
            CHAPTER_HINTS
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

    use super::*;
    use crate::db::Progress;
    use crate::model::SourceId;
    use crate::tui::app::AppEvent;
    use crate::tui::app::test_data::*;

    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect()
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
        let (mut app, _) = App::new(crate::tui::Start::Search);
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
    fn renders_in_tiny_terminal() {
        let mut app = searched();
        render(&mut app, 20, 5);
        let mut app = chapters(read(1..=50));
        render(&mut app, 20, 5);
        for (w, h) in [(1, 1), (0, 0), (80, 2)] {
            render(&mut app, w, h);
        }
    }
}
