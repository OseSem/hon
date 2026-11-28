//! The library, search and series screens.

use hon_core::db::{LibraryEntry, Progress, resume_page};
use hon_core::model::SourceId;
use hon_core::search::TitleGroup;
use iced::widget::{
    Column, button, center, column, container, grid, pick_list, row, scrollable, space, stack,
    text, text_input,
};
use iced::{Alignment, Color, ContentFit, Element, Length, Padding};

use crate::theme::{self, FAINT, MUTED, PAPER};
use crate::{App, Message, SEARCH_INPUT, SeriesView};

/// A source in the series' source picker.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceChoice(pub SourceId);

impl std::fmt::Display for SourceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.name())
    }
}

const PAGE_MAX: f32 = 1240.0;
const CARD_W: f32 = 176.0;
/// Covers are drawn at AniList's 1:1.42 ratio.
const COVER_RATIO: f32 = 1.42;

pub fn failed_to_start(error: &str) -> Element<'_, Message> {
    center(
        column![
            text("hon couldn't start").font(theme::DISPLAY).size(28),
            text(error).size(14).color(MUTED),
        ]
        .spacing(10)
        .max_width(560),
    )
    .into()
}

/// The screen, with any notes over its bottom edge.
pub fn with_notes<'a>(screen: Element<'a, Message>, notes: &'a [String]) -> Element<'a, Message> {
    if notes.is_empty() {
        return screen;
    }
    let toast = container(
        row![
            text(notes.join("\n")).size(13).width(Length::Fill),
            button(text("Dismiss").size(12))
                .on_press(Message::Dismiss)
                .padding([4, 10])
                .style(theme::ghost),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .padding([12, 16])
    .max_width(640)
    .style(theme::toast);
    let overlay = container(toast)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::End)
        .padding(24);
    stack![screen, overlay].into()
}

/// Keeps content to a readable width, centered in the window.
fn page<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let inner = container(content)
        .max_width(PAGE_MAX)
        .width(Length::Fill)
        .padding(Padding {
            top: 28.0,
            right: 40.0,
            bottom: 56.0,
            left: 40.0,
        });
    scrollable(container(inner).center_x(Length::Fill))
        .height(Length::Fill)
        .style(theme::scroll)
        .into()
}

fn search_bar(app: &App) -> Element<'_, Message> {
    let input = text_input("Search your sources", &app.query)
        .id(SEARCH_INPUT)
        .on_input(Message::Query)
        .on_submit(Message::Search)
        .padding([10, 14])
        .size(15)
        .style(theme::search);
    let mut bar = row![
        text("hon").font(theme::DISPLAY).size(30),
        space().width(Length::Fill),
        container(input).max_width(440),
    ]
    .spacing(16)
    .align_y(Alignment::Center);
    if app.outcome.is_some() || app.searching {
        bar = bar.push(
            button(text("Library").size(13))
                .on_press(Message::ClearSearch)
                .padding([8, 12])
                .style(theme::ghost),
        );
    }
    bar.into()
}

pub fn home<'a>(app: &'a App, no_sources: Option<&'a str>) -> Element<'a, Message> {
    let mut content = Column::new().spacing(36).push(search_bar(app));
    if let Some(msg) = no_sources {
        return page(content.push(empty("No sources yet", msg)));
    }
    if app.searching || app.outcome.is_some() {
        return page(content.push(results(app)));
    }
    let Some((first, rest)) = app.library.split_first() else {
        return page(content.push(empty(
            "Nothing read yet",
            "Search for a manga above, then pick a chapter to start.",
        )));
    };
    content = content.push(continue_card(app, first));
    if !rest.is_empty() {
        let cards = rest.iter().map(|e| {
            card(
                app,
                &e.series.title,
                entry_status(e),
                entry_progress(e),
                Message::OpenEntry {
                    id: e.series.id,
                    read: false,
                },
            )
        });
        content = content.push(
            column![
                text("Library").font(theme::DISPLAY).size(22),
                cover_grid(cards.collect()),
            ]
            .spacing(16),
        );
    }
    page(content)
}

fn empty<'a>(title: &'a str, body: &'a str) -> Element<'a, Message> {
    column![
        text(title).font(theme::DISPLAY).size(26),
        text(body).size(14).color(MUTED),
    ]
    .spacing(8)
    .padding([48, 0])
    .into()
}

fn results(app: &App) -> Element<'_, Message> {
    let mut col = Column::new().spacing(18);
    let heading = if app.searching {
        format!("Searching for \u{201c}{}\u{201d}", app.query.trim())
    } else {
        format!("Results for \u{201c}{}\u{201d}", app.query.trim())
    };
    col = col.push(text(heading).font(theme::DISPLAY).size(22));
    let Some(outcome) = &app.outcome else {
        return col.into();
    };
    for (source, error) in &outcome.failures {
        col = col.push(
            text(format!("{} didn't answer: {error}", source.name()))
                .size(12)
                .color(FAINT),
        );
    }
    if outcome.groups.is_empty() {
        if !app.searching {
            col = col.push(
                text("No source has a title like that. Try another spelling or fewer words.")
                    .size(14)
                    .color(MUTED),
            );
        }
        return col.into();
    }
    let cards = outcome
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| card(app, &g.title, group_status(g), None, Message::OpenResult(i)))
        .collect();
    col.push(cover_grid(cards)).into()
}

fn cover_grid(cards: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    grid(cards)
        .fluid(CARD_W)
        .spacing(28)
        .height(grid::aspect_ratio(CARD_W, CARD_W * COVER_RATIO + 64.0))
        .into()
}

/// How far into its chapter a library series is, for the strip along its cover.
fn entry_progress(e: &LibraryEntry) -> Option<(u32, u32)> {
    match (e.progress.read, e.progress.last_page, e.progress.page_count) {
        (true, ..) => None,
        (false, Some(p), Some(n)) => Some((p + 1, n)),
        _ => None,
    }
}

fn card<'a>(
    app: &'a App,
    title: &'a str,
    status: String,
    progress: Option<(u32, u32)>,
    msg: Message,
) -> Element<'a, Message> {
    let picture = cover(app, title, Length::Fill, Length::Fill, 28.0);
    let picture: Element<'a, Message> = match progress {
        Some((done, total)) => {
            let accent = theme::accent(cover_color(app, title));
            let backing = container(meter(done, total, accent))
                .padding([6, 8])
                .style(theme::fill(theme::alpha(theme::INK, 0.78), 4.0));
            let strip = container(backing)
                .padding([8, 8])
                .width(Length::Fill)
                .height(Length::Fill)
                .align_y(Alignment::End);
            stack![picture, strip].into()
        }
        None => picture,
    };
    let body = column![
        picture,
        column![
            text(title)
                .size(14)
                .wrapping(text::Wrapping::WordOrGlyph)
                .height(20),
            text(status).size(12).color(MUTED),
        ]
        .spacing(2),
    ]
    .spacing(10);
    button(body)
        .on_press(msg)
        .padding(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::card)
        .into()
}

/// A series' cover, or its initial on a plain block while there's none.
fn cover<'a>(
    app: &'a App,
    title: &str,
    width: Length,
    height: Length,
    initial: f32,
) -> Element<'a, Message> {
    if let Some(handle) = app.cover(title).and_then(|c| c.image.clone()) {
        return iced::widget::image(handle)
            .width(width)
            .height(height)
            .content_fit(ContentFit::Cover)
            .border_radius(6)
            .into();
    }
    let letter: String = title
        .chars()
        .find(|c| c.is_alphanumeric())
        .into_iter()
        .collect();
    container(text(letter).font(theme::DISPLAY).size(initial).color(FAINT))
        .center(Length::Fill)
        .width(width)
        .height(height)
        .style(theme::fill(theme::SURFACE, 6.0))
        .into()
}

fn cover_color(app: &App, title: &str) -> Option<Color> {
    app.cover(title).and_then(|c| c.color)
}

fn continue_card<'a>(app: &'a App, e: &'a LibraryEntry) -> Element<'a, Message> {
    let title = &e.series.title;
    let accent = theme::accent(cover_color(app, title));
    let where_ = match (&e.progress, e.progress.read) {
        (_, true) => format!("Finished chapter {}", e.chapter),
        (
            Progress {
                last_page: Some(p),
                page_count: Some(n),
                ..
            },
            _,
        ) => {
            format!("Chapter {}, page {} of {n}", e.chapter, p + 1)
        }
        _ => format!("Chapter {}", e.chapter),
    };
    let mut lines = column![
        text(title).font(theme::DISPLAY).size(40),
        text(where_).size(15).color(PAPER),
    ]
    .spacing(6);
    if let (false, Some(p), Some(n)) =
        (e.progress.read, e.progress.last_page, e.progress.page_count)
    {
        lines = lines.push(
            container(meter(p + 1, n, accent))
                .max_width(320)
                .padding([6, 0]),
        );
    }
    let mut when = relative(e.progress.updated_at);
    if let Some(source) = &e.progress.source {
        when += &format!(" on {}", source_name(app, source));
    }
    lines = lines.push(text(format!("Last read {when}")).size(12).color(FAINT));
    let actions = row![
        button(text("Continue reading").font(theme::STRONG).size(14))
            .on_press(Message::OpenEntry {
                id: e.series.id,
                read: true
            })
            .padding([10, 22])
            .style(theme::paper),
        button(text("All chapters").size(14))
            .on_press(Message::OpenEntry {
                id: e.series.id,
                read: false
            })
            .padding([10, 16])
            .style(theme::ghost),
    ]
    .spacing(8);
    let info = column![lines, actions].spacing(24).width(Length::Fill);
    container(
        row![
            cover(app, title, 190.into(), (190.0 * COVER_RATIO).into(), 64.0),
            info
        ]
        .spacing(32)
        .align_y(Alignment::Center),
    )
    .padding(28)
    .width(Length::Fill)
    .style(theme::wash(cover_color(app, title)))
    .into()
}

/// How far along a chapter or series is, as a thin line.
fn meter<'a>(done: u32, total: u32, accent: Color) -> Element<'a, Message> {
    let total = total.max(1);
    let done = done.min(total);
    row![
        container(space())
            .width(Length::FillPortion(done as u16))
            .height(3)
            .style(theme::line(accent)),
        container(space())
            .width(Length::FillPortion((total - done) as u16))
            .height(3)
            .style(theme::line(theme::RAISED)),
    ]
    .spacing(if done > 0 && done < total { 3 } else { 0 })
    .into()
}

fn entry_status(e: &LibraryEntry) -> String {
    match (e.progress.read, e.progress.last_page, e.progress.page_count) {
        (true, ..) => format!("Read chapter {}", e.chapter),
        (false, Some(p), Some(n)) => format!("Chapter {}, page {}/{n}", e.chapter, p + 1),
        _ => format!("Chapter {}", e.chapter),
    }
}

fn group_status(g: &TitleGroup) -> String {
    match g.sources.first().and_then(|s| s.latest) {
        Some(latest) => format!("Up to chapter {latest}"),
        None => "No chapters yet".into(),
    }
}

fn source_name(app: &App, slug: &str) -> String {
    let Ok(ctx) = &app.ctx else {
        return slug.into();
    };
    ctx.sources
        .iter()
        .map(|s| s.id())
        .find(|id| id.slug() == slug)
        .map(|id| id.name().to_string())
        .unwrap_or_else(|| slug.into())
}

/// "today", "yesterday", "3 days ago", "2 weeks ago", else the date.
pub fn relative(unix: i64) -> String {
    relative_to(unix, jiff::Timestamp::now().as_second())
}

fn relative_to(unix: i64, now: i64) -> String {
    let days = (now - unix).max(0) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        2..=13 => format!("{days} days ago"),
        14..=59 => format!("{} weeks ago", days / 7),
        _ => jiff::Timestamp::from_second(unix)
            .map(|t| format!("on {}", t.strftime("%B %-d, %Y")))
            .unwrap_or_default(),
    }
}

pub fn series<'a>(app: &'a App, view: &'a SeriesView) -> Element<'a, Message> {
    let title = &view.series.title;
    let color = cover_color(app, title);
    let accent = theme::accent(color);
    let chapters = view.chapters();

    let mut info = column![text(title).font(theme::DISPLAY).size(42)].spacing(14);
    if let (Some(group), Some(source)) = (&view.group, &view.source) {
        let choices: Vec<SourceChoice> = group
            .sources
            .iter()
            .map(|s| SourceChoice(s.manga.source.clone()))
            .collect();
        let picker = pick_list(
            choices,
            Some(SourceChoice(source.clone())),
            Message::PickSource,
        )
        .text_size(13)
        .padding([5, 10])
        .style(theme::picker)
        .menu_style(theme::menu);
        info = info.push(
            row![
                text(format!("{} chapters on", chapters.len()))
                    .size(14)
                    .color(MUTED),
                picker
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        let read = view.read_count();
        info = info.push(
            column![
                text(format!("{read} of {} read", chapters.len()))
                    .size(13)
                    .color(MUTED),
                container(meter(read as u32, chapters.len() as u32, accent)).max_width(360),
            ]
            .spacing(8),
        );
        if let Some((i, page)) = view.resume() {
            let ch = &chapters[i];
            let label = match (page, view.progress.get(&ch.number)) {
                (0, Some(p)) if p.read => format!("Read chapter {} again", ch.number),
                (0, _) if i == 0 && view.progress.is_empty() => "Start reading".to_string(),
                (0, _) => format!("Read chapter {}", ch.number),
                (p, _) => format!("Continue chapter {}, page {}", ch.number, p + 1),
            };
            info = info.push(
                button(text(label).font(theme::STRONG).size(14))
                    .on_press(Message::Continue)
                    .padding([10, 22])
                    .style(theme::paper),
            );
        }
    } else if let Some(e) = &view.failed {
        info = info.push(text(e).size(14).color(MUTED));
    } else {
        info = info.push(text("Looking it up on your sources").size(14).color(MUTED));
    }

    let header = container(
        row![
            cover(app, title, 210.into(), (210.0 * COVER_RATIO).into(), 72.0),
            info.width(Length::Fill)
        ]
        .spacing(36)
        .align_y(Alignment::Center),
    )
    .padding(28)
    .width(Length::Fill)
    .style(theme::wash(color));

    let back = button(text("Back").size(13))
        .on_press(Message::Back)
        .padding([6, 12])
        .style(theme::ghost);
    let mut content = column![back, header].spacing(18);
    if !chapters.is_empty() {
        content = content.push(chapter_list(app, view, accent));
    }
    page(content)
}

fn chapter_list<'a>(app: &'a App, view: &'a SeriesView, accent: Color) -> Element<'a, Message> {
    let Some(source) = &view.source else {
        return space().into();
    };
    let order = if app.newest_first {
        "Newest first"
    } else {
        "Oldest first"
    };
    let mut list = Column::new().spacing(2);
    let chapters = view.chapters();
    let indices: Box<dyn Iterator<Item = usize>> = if app.newest_first {
        Box::new((0..chapters.len()).rev())
    } else {
        Box::new(0..chapters.len())
    };
    for i in indices {
        let ch = &chapters[i];
        let progress = view.progress.get(&ch.number);
        let read = progress.is_some_and(|p| p.read);
        let in_progress = match progress {
            Some(p) if !p.read && p.source.as_deref() == Some(source.slug()) => {
                match (p.last_page, p.page_count) {
                    (Some(page), Some(count)) => Some(format!("Page {} of {count}", page + 1)),
                    _ => None,
                }
            }
            _ => None,
        };
        let ink = if read { FAINT } else { PAPER };
        let date = ch
            .published
            .map(|t| t.strftime("%b %-d, %Y").to_string())
            .unwrap_or_default();
        let label = row![
            text(ch.number.to_string())
                .font(theme::SERIF)
                .size(17)
                .color(ink)
                .width(64),
            text(ch.title.clone().unwrap_or_default())
                .size(14)
                .color(if read { FAINT } else { MUTED })
                .width(Length::Fill)
                .wrapping(text::Wrapping::None),
            text(in_progress.unwrap_or_default()).size(12).color(accent),
            text(date).size(12).color(FAINT).width(96),
        ]
        .spacing(14)
        .align_y(Alignment::Center);
        let page = if read {
            0
        } else {
            resume_page(progress, source)
        };
        let open = button(label)
            .on_press(Message::Read(i, page))
            .width(Length::Fill)
            .padding([9, 12])
            .style(theme::row);
        let mark = button(
            text(if read { "Read" } else { "Mark read" })
                .size(12)
                .color(if read { accent } else { FAINT }),
        )
        .on_press(Message::ToggleRead(ch.number))
        .padding([6, 10])
        .width(88)
        .style(theme::ghost);
        list = list.push(row![open, mark].spacing(4).align_y(Alignment::Center));
    }
    column![
        row![
            text("Chapters").font(theme::DISPLAY).size(22),
            space().width(Length::Fill),
            button(text(order).size(12))
                .on_press(Message::SortChapters)
                .padding([5, 10])
                .style(theme::ghost),
        ]
        .align_y(Alignment::Center),
        list,
    ]
    .spacing(10)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_read_naturally() {
        let now = 1_800_000_000;
        assert_eq!(relative_to(now - 60, now), "today");
        assert_eq!(relative_to(now - 86_400 - 5, now), "yesterday");
        assert_eq!(relative_to(now - 3 * 86_400, now), "3 days ago");
        assert_eq!(relative_to(now - 20 * 86_400, now), "2 weeks ago");
        assert!(relative_to(now - 90 * 86_400, now).starts_with("on "));
        assert_eq!(relative_to(now + 500, now), "today");
    }
}
