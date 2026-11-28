//! Everything but the reader: the sidebar and top bar, home, discover, library, a series
//! and settings.

use std::time::Duration;

use hon::commands::Ctx;
use hon_core::db::{LibraryEntry, resume_page};
use hon_core::model::SourceId;
use hon_core::search::TitleGroup;
use iced::widget::{
    Column, Row, button, center, column, container, grid, image, mouse_area, opaque, pick_list,
    row, scrollable, space, stack, text, text_input, tooltip,
};
use iced::{Alignment, Color, ContentFit, Element, Length, Padding};

use crate::covers::Cover;
use crate::settings::{Backdrop, Fit, Mode, Sort, View};
use crate::theme::{self, ACCENT, FAINT, MUTED, TEXT, icon};
use crate::{App, Message, Pref, SEARCH_INPUT, Screen, SeriesView, Shelf};

/// A source in the series' source picker.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceChoice(pub SourceId);

impl std::fmt::Display for SourceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.name())
    }
}

const CONTENT_MAX: f32 = 1360.0;
const SIDEBAR_W: f32 = 244.0;
const CARD_W: f32 = 172.0;
/// Covers are drawn at AniList's 1:1.42 ratio.
const COVER_RATIO: f32 = 1.42;
const HERO_H: f32 = 430.0;
/// Room for the window buttons, which sit over the content on macOS.
const TITLEBAR: f32 = if cfg!(target_os = "macos") { 30.0 } else { 0.0 };
/// How much of a description shows before "More".
const SYNOPSIS: usize = 360;
/// Keys and what they do in the reader.
const SHORTCUTS: &[(&str, &str)] = &[
    ("Space, Page Down", "Next screen or page"),
    ("Shift+Space, Page Up", "Previous screen or page"),
    ("↓ ↑, j k", "Scroll, or turn a page"),
    ("← →", "Turn a page, or change chapter while scrolling"),
    ("n, p", "Next or previous chapter"),
    ("Home, End", "First or last page"),
    ("m", "Switch between scrolling and pages"),
    ("d", "Two pages side by side"),
    ("t", "Page thumbnails"),
    ("f", "Hide or show the bars"),
    ("+ −", "Wider or narrower strip"),
    ("/", "Search"),
    ("?", "These shortcuts"),
    ("Esc", "Back"),
];

pub fn failed_to_start(error: &str) -> Element<'_, Message> {
    center(
        column![
            text("hon couldn't start").font(theme::BLACK).size(30),
            text(error).size(14).color(MUTED),
        ]
        .spacing(10)
        .max_width(560),
    )
    .into()
}

pub fn no_sources(msg: &str) -> Element<'_, Message> {
    scroll_page(
        column![
            text("Add a source to start").font(theme::BLACK).size(36),
            text(msg).size(15).color(MUTED),
            text("Sources come from plugin repos other people maintain. hon shows what they find.")
                .size(13)
                .color(FAINT),
        ]
        .spacing(12)
        .padding([60, 0]),
    )
}

/// A hint that shows when the pointer rests on `content`.
pub fn tip<'a>(
    content: impl Into<Element<'a, Message>>,
    label: &'a str,
    position: tooltip::Position,
) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(label).size(12)).padding([5, 9]),
        position,
    )
    .gap(6)
    .delay(Duration::from_millis(350))
    .style(theme::tooltip)
    .into()
}

/// The sidebar and top bar around a screen.
pub fn shell<'a>(app: &'a App, ctx: &'a Ctx, main: Element<'a, Message>) -> Element<'a, Message> {
    row![sidebar(app, ctx), column![topbar(app), main]].into()
}

fn topbar(app: &App) -> Element<'_, Message> {
    let input = text_input("Search manga", &app.query)
        .id(SEARCH_INPUT)
        .on_input(Message::Query)
        .on_submit(Message::Search)
        .icon(text_input::Icon {
            font: theme::ICONS,
            code_point: icon::SEARCH,
            size: Some(15.0.into()),
            spacing: 10.0,
            side: text_input::Side::Left,
        })
        .padding([9, 16])
        .size(14)
        .width(380)
        .style(theme::search);
    let help = button(icon(icon::KEYBOARD, 17.0))
        .on_press(Message::Help)
        .padding(9)
        .style(theme::round_icon(false));
    let bar = row![
        input,
        space().width(Length::Fill),
        tip(help, "Keyboard shortcuts", tooltip::Position::Bottom),
    ]
    .align_y(Alignment::Center)
    .padding(Padding {
        top: 14.0,
        right: 28.0,
        bottom: 12.0,
        left: 36.0,
    });
    mouse_area(container(bar).width(Length::Fill))
        .on_press(Message::Drag)
        .into()
}

fn sidebar<'a>(app: &'a App, ctx: &'a Ctx) -> Element<'a, Message> {
    let current = match (&app.series, app.screen) {
        (Some(_), _) => None,
        (None, Screen::Library(_)) => Some(Screen::Library(Shelf::All)),
        (None, screen) => Some(screen),
    };
    let item = |glyph: char, label: &'a str, screen: Screen| {
        let active = current == Some(screen);
        button(
            row![
                icon(glyph, 18.0).color(if active { ACCENT } else { MUTED }),
                text(label).font(theme::SEMI).size(14),
            ]
            .spacing(14)
            .align_y(Alignment::Center),
        )
        .on_press(Message::Nav(screen))
        .width(Length::Fill)
        .padding([10, 14])
        .style(theme::nav(active))
    };
    let brand = mouse_area(
        row![
            container(text("本").size(15).color(theme::ON_ACCENT))
                .center(28)
                .style(theme::fill(ACCENT, 8.0)),
            text("hon").font(theme::BLACK).size(24),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding(Padding {
            top: TITLEBAR + 18.0,
            right: 14.0,
            bottom: 6.0,
            left: 14.0,
        })
        .width(Length::Fill),
    )
    .on_press(Message::Drag);
    let nav = column![
        item(icon::HOME, "Home", Screen::Home),
        item(icon::COMPASS, "Discover", Screen::Discover),
        item(icon::LIBRARY, "Library", Screen::Library(Shelf::All)),
    ]
    .spacing(2);

    let mut recent = Column::new().spacing(2);
    for e in app.library.iter().take(7) {
        let title = &e.series.title;
        let status = match (e.progress.read, e.progress.last_page) {
            (true, _) => format!("Read chapter {}", e.chapter),
            (false, Some(p)) => format!("Chapter {}, page {}", e.chapter, p + 1),
            _ => format!("Chapter {}", e.chapter),
        };
        let line = row![
            cover(
                app,
                title,
                36.0.into(),
                (36.0 * COVER_RATIO).into(),
                14.0,
                5.0
            ),
            column![
                text(title)
                    .font(theme::SEMI)
                    .size(13)
                    .wrapping(text::Wrapping::None),
                text(status).size(11).color(FAINT),
            ]
            .spacing(2),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        let open = app
            .series
            .as_ref()
            .is_some_and(|v| v.series.id == e.series.id);
        recent = recent.push(
            button(line)
                .on_press(Message::OpenEntry {
                    id: e.series.id,
                    read: false,
                })
                .width(Length::Fill)
                .padding([6, 10])
                .clip(true)
                .style(theme::nav(open)),
        );
    }
    let mut body = column![nav].spacing(22);
    if !app.library.is_empty() {
        body = body.push(
            column![
                container(
                    text("Recently read")
                        .font(theme::BOLD)
                        .size(12)
                        .color(FAINT)
                )
                .padding([0, 14]),
                recent
            ]
            .spacing(8),
        );
    }
    let sources = match ctx.sources.len() {
        1 => "1 source".to_string(),
        n => format!("{n} sources"),
    };
    let settings = button(
        row![
            icon(icon::SETTINGS, 18.0).color(if current == Some(Screen::Settings) {
                ACCENT
            } else {
                MUTED
            }),
            text("Settings")
                .font(theme::SEMI)
                .size(14)
                .width(Length::Fill),
            text(sources).size(11).color(FAINT),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .on_press(Message::Nav(Screen::Settings))
    .width(Length::Fill)
    .padding([10, 14])
    .style(theme::nav(current == Some(Screen::Settings)));
    container(
        column![
            brand,
            scrollable(body.padding([10, 10]))
                .style(theme::scroll)
                .height(Length::Fill),
            container(settings).padding([10, 10]),
        ]
        .height(Length::Fill),
    )
    .width(SIDEBAR_W)
    .height(Length::Fill)
    .style(theme::sidebar)
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
            button(text("Dismiss").font(theme::SEMI).size(12))
                .on_press(Message::Dismiss)
                .padding([5, 12])
                .style(theme::ghost),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .padding([12, 16])
    .max_width(640)
    .style(theme::floating);
    let overlay = container(toast)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::End)
        .padding(24);
    stack![screen, overlay].into()
}

/// The keyboard shortcuts over the screen.
pub fn help(screen: Element<'_, Message>) -> Element<'_, Message> {
    let card = container(
        column![
            row![
                text("Keyboard shortcuts")
                    .font(theme::HEAVY)
                    .size(22)
                    .width(Length::Fill),
                button(icon(icon::CLOSE, 16.0))
                    .on_press(Message::Help)
                    .padding(8)
                    .style(theme::round_icon(false)),
            ]
            .align_y(Alignment::Center),
            shortcut_table(),
        ]
        .spacing(18),
    )
    .padding(28)
    .max_width(560)
    .style(theme::floating);
    let scrim = mouse_area(
        container(opaque(card))
            .center(Length::Fill)
            .style(theme::fill(theme::alpha(theme::DEEP, 0.78), 0.0)),
    )
    .on_press(Message::Help);
    stack![screen, opaque(scrim)].into()
}

fn shortcut_table<'a>() -> Element<'a, Message> {
    let mut rows = Column::new().spacing(10);
    for (keys, what) in SHORTCUTS {
        rows = rows.push(
            row![
                text(*keys).font(theme::BOLD).size(13).width(180),
                text(*what).size(13).color(MUTED),
            ]
            .spacing(16),
        );
    }
    rows.into()
}

/// Keeps content to a readable width, centered, scrolling.
fn scroll_page<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    scrollable(container(padded(content)).center_x(Length::Fill))
        .height(Length::Fill)
        .width(Length::Fill)
        .style(theme::scroll)
        .into()
}

fn padded<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .max_width(CONTENT_MAX)
        .width(Length::Fill)
        .padding(Padding {
            top: 12.0,
            right: 36.0,
            bottom: 72.0,
            left: 36.0,
        })
        .into()
}

/// A header over the series' artwork (its banner, else its cover blurred, else its
/// color), fading into the background below. `content` sits along its bottom.
fn backdrop<'a>(app: &'a App, title: &str, content: Element<'a, Message>) -> Element<'a, Message> {
    let art = app.cover(title);
    let picture = |handle: image::Handle, opacity: f32| -> Element<'a, Message> {
        image(handle)
            .width(Length::Fill)
            .height(Length::Fill)
            .content_fit(ContentFit::Cover)
            .opacity(opacity)
            .into()
    };
    let wash: Element<'a, Message> = match art {
        Some(Cover {
            banner: Some(banner),
            ..
        }) => picture(banner.clone(), 0.95),
        Some(Cover {
            backdrop: Some(soft),
            color,
            ..
        }) => stack![
            picture(soft.clone(), 1.0),
            container(space())
                .width(Length::Fill)
                .height(Length::Fill)
                .style(theme::fill(theme::alpha(theme::deep(*color), 0.55), 0.0)),
        ]
        .into(),
        _ => container(space())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::wash(art.and_then(|c| c.color)))
            .into(),
    };
    let fade = |style: fn(&iced::Theme) -> container::Style| {
        container(space())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(style)
    };
    let content = container(
        container(content)
            .max_width(CONTENT_MAX)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(Padding {
                top: 18.0,
                right: 36.0,
                bottom: 30.0,
                left: 36.0,
            }),
    )
    .center_x(Length::Fill)
    .height(Length::Fill);
    container(stack![
        wash,
        fade(theme::fade_side),
        fade(theme::fade_down),
        content
    ])
    .width(Length::Fill)
    .height(HERO_H)
    .clip(true)
    .into()
}

fn heading<'a>(glyph: Option<char>, label: &'a str) -> Row<'a, Message> {
    let mut line = Row::new().spacing(10).align_y(Alignment::Center);
    if let Some(glyph) = glyph {
        line = line.push(icon(glyph, 19.0).color(ACCENT));
    }
    line.push(text(label).font(theme::HEAVY).size(22))
}

pub fn home(app: &App) -> Element<'_, Message> {
    let Some((first, rest)) = app.library.split_first() else {
        return discover(app);
    };
    let mut body = Column::new().spacing(44);
    let reading: Vec<&LibraryEntry> = rest
        .iter()
        .filter(|e| !e.progress.read && e.progress.last_page.is_some())
        .collect();
    if !reading.is_empty() {
        let tiles: Vec<Element<'_, Message>> =
            reading.into_iter().take(6).map(|e| tile(app, e)).collect();
        body = body.push(
            column![
                heading(None, "Jump back in"),
                grid(tiles)
                    .fluid(340)
                    .spacing(14)
                    .height(grid::Sizing::EvenlyDistribute(Length::Shrink)),
            ]
            .spacing(16),
        );
    }
    if !app.trending.is_empty() {
        body = body.push(trending_row(app));
    }
    let more = rest.len() > 12;
    let mut header = heading(None, "Your library").width(Length::Fill);
    if more {
        header = header.push(
            button(text("Show all").font(theme::SEMI).size(13))
                .on_press(Message::Nav(Screen::Library(Shelf::All)))
                .padding([6, 12])
                .style(theme::ghost),
        );
    }
    let cards = app
        .library
        .iter()
        .take(12)
        .map(|e| entry_card(app, e))
        .collect();
    body = body.push(column![header, cover_grid(cards)].spacing(16));
    scrollable(column![
        hero(app, first),
        container(padded(body)).center_x(Length::Fill)
    ])
    .height(Length::Fill)
    .style(theme::scroll)
    .into()
}

fn hero<'a>(app: &'a App, e: &'a LibraryEntry) -> Element<'a, Message> {
    let title = &e.series.title;
    let color = theme::vivid(cover_color(app, title));
    let where_ = match (e.progress.read, e.progress.last_page, e.progress.page_count) {
        (true, ..) => format!("You finished chapter {}", e.chapter),
        (false, Some(p), Some(n)) => format!("Chapter {}, page {} of {n}", e.chapter, p + 1),
        _ => format!("Chapter {}", e.chapter),
    };
    let mut when = format!("Last read {}", relative(e.progress.updated_at));
    if let Some(source) = &e.progress.source {
        when += &format!(" on {}", source_name(app, source));
    }
    let mut info = column![
        row![
            icon(icon::BOOK, 15.0).color(ACCENT),
            text("Continue reading")
                .font(theme::BOLD)
                .size(13)
                .color(ACCENT),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        text(title).font(theme::BLACK).size(52).line_height(1.02),
        text(where_).font(theme::SEMI).size(17),
    ]
    .spacing(10)
    .max_width(760);
    if let (false, Some(p), Some(n)) =
        (e.progress.read, e.progress.last_page, e.progress.page_count)
    {
        info = info.push(container(meter(p + 1, n, color, 5.0)).max_width(360));
    }
    info = info.push(text(when).size(13).color(MUTED));
    let actions = row![
        button(
            row![
                icon(icon::PLAY, 15.0),
                text("Continue").font(theme::BOLD).size(15)
            ]
            .spacing(9)
            .align_y(Alignment::Center)
        )
        .on_press(Message::OpenEntry {
            id: e.series.id,
            read: true
        })
        .padding([12, 26])
        .style(theme::primary),
        button(text("Chapters").font(theme::BOLD).size(15))
            .on_press(Message::OpenEntry {
                id: e.series.id,
                read: false
            })
            .padding([12, 22])
            .style(theme::secondary),
        favorite_button(app, e.series.id),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let content = column![
        space().height(Length::Fill),
        row![
            container(cover(
                app,
                title,
                168.0.into(),
                (168.0 * COVER_RATIO).into(),
                56.0,
                10.0
            ))
            .style(theme::shadow(10.0)),
            column![info, actions].spacing(22).width(Length::Fill),
        ]
        .spacing(32)
        .align_y(Alignment::End),
    ]
    .height(Length::Fill)
    .into();
    backdrop(app, title, content)
}

fn favorite_button(app: &App, id: i64) -> Element<'_, Message> {
    let on = app.is_favorite(id);
    let heart = button(icon(icon::HEART, 18.0))
        .on_press(Message::ToggleFavorite(id))
        .padding(13)
        .style(move |theme, status| {
            if on {
                theme::round_icon(true)(theme, status)
            } else {
                theme::secondary(theme, status)
            }
        });
    tip(
        heart,
        if on {
            "In your favorites"
        } else {
            "Add to favorites"
        },
        tooltip::Position::Top,
    )
}

/// A wide card for a series in progress: cover, where reading is, and a meter.
fn tile<'a>(app: &'a App, e: &'a LibraryEntry) -> Element<'a, Message> {
    let title = &e.series.title;
    let color = theme::vivid(cover_color(app, title));
    let mut words = column![
        text(title)
            .font(theme::BOLD)
            .size(15)
            .wrapping(text::Wrapping::None),
        text(entry_status(e)).size(12).color(MUTED),
    ]
    .spacing(3)
    .width(Length::Fill);
    if let Some((done, total)) = entry_progress(e) {
        words = words
            .push(space().height(6))
            .push(meter(done, total, color, 4.0));
    }
    let play = container(icon(icon::PLAY, 15.0).color(theme::ON_ACCENT))
        .center(38)
        .style(theme::fill(ACCENT, 100.0));
    button(
        row![
            cover(
                app,
                title,
                56.0.into(),
                (56.0 * COVER_RATIO).into(),
                20.0,
                6.0
            ),
            words,
            play
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .on_press(Message::OpenEntry {
        id: e.series.id,
        read: true,
    })
    .padding(12)
    .width(Length::Fill)
    .clip(true)
    .style(theme::tile)
    .into()
}

fn trending_row(app: &App) -> Element<'_, Message> {
    let cards = app.trending.iter().take(16).map(|t| {
        container(card(
            app,
            &t.title,
            trend_note(app, &t.title),
            None,
            Message::SearchFor(t.title.clone()),
        ))
        .width(CARD_W + 16.0)
        .height(card_height())
        .into()
    });
    let rail = scrollable::Scrollbar::new().width(4).scroller_width(4);
    column![
        heading(Some(icon::FLAME), "Trending now")
            .width(Length::Fill)
            .push(
                button(text("Show all").font(theme::SEMI).size(13))
                    .on_press(Message::Nav(Screen::Discover))
                    .padding([6, 12])
                    .style(theme::ghost)
            ),
        scrollable(
            Row::with_children(cards)
                .spacing(4)
                .padding(Padding::default().bottom(14))
        )
        .direction(scrollable::Direction::Horizontal(rail))
        .style(theme::scroll),
    ]
    .spacing(12)
    .into()
}

/// A trending title's genres, as the line under its name.
fn trend_note(app: &App, title: &str) -> String {
    app.cover(title)
        .map(|c| {
            c.info
                .genres
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

pub fn discover(app: &App) -> Element<'_, Message> {
    let mut col = Column::new().spacing(18);
    let searched = app.searching || app.outcome.is_some();
    if !searched {
        col = col.push(
            column![
                text("Discover").font(theme::BLACK).size(38),
                text("What people are reading this week. Pick one to find it on your sources.")
                    .size(14)
                    .color(MUTED),
            ]
            .spacing(4),
        );
        if app.trending.is_empty() {
            let hint = if app.prefs.covers {
                "Search for a title above to find it on your sources."
            } else {
                "Trending needs AniList, which is off in Settings. Search above still works."
            };
            return scroll_page(col.push(text(hint).size(14).color(FAINT)));
        }
        let cards = app
            .trending
            .iter()
            .map(|t| {
                card(
                    app,
                    &t.title,
                    trend_note(app, &t.title),
                    None,
                    Message::SearchFor(t.title.clone()),
                )
            })
            .collect();
        return scroll_page(col.push(space().height(6)).push(cover_grid(cards)));
    }
    let query = app.query.trim();
    let title = if app.searching {
        format!("Searching for \u{201c}{query}\u{201d}")
    } else {
        format!("Results for \u{201c}{query}\u{201d}")
    };
    col = col.push(
        row![
            text(title).font(theme::BLACK).size(34).width(Length::Fill),
            button(text("Clear").font(theme::SEMI).size(13))
                .on_press(Message::ClearSearch)
                .padding([7, 14])
                .style(theme::tab(false)),
        ]
        .align_y(Alignment::Center),
    );
    let Some(outcome) = &app.outcome else {
        return scroll_page(col.push(text("Asking your sources").size(14).color(MUTED)));
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
                text("No source has a title like that. Try another spelling, or fewer words.")
                    .size(15)
                    .color(MUTED),
            );
        }
        return scroll_page(col);
    }
    let cards = outcome
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| result_card(app, g, i))
        .collect();
    scroll_page(col.push(cover_grid(cards)))
}

fn result_card<'a>(app: &'a App, g: &'a TitleGroup, i: usize) -> Element<'a, Message> {
    let names: Vec<&str> = g.sources.iter().map(|s| s.manga.source.name()).collect();
    let chapters = match g.sources.first().and_then(|s| s.latest) {
        Some(latest) => format!("Up to chapter {latest} on {}", names.join(", ")),
        None => format!("No chapters on {}", names.join(", ")),
    };
    card(app, &g.title, chapters, None, Message::OpenResult(i))
}

pub fn library(app: &App, shelf: Shelf) -> Element<'_, Message> {
    let entries = app.shelf(shelf);
    let empty = match shelf {
        Shelf::All => "Series you start reading show up here.",
        Shelf::Reading => "Nothing half read. Every chapter you opened, you finished.",
        Shelf::Favorites => "Tap the heart on a series to keep it here.",
    };
    let tab = |label: &'static str, to: Shelf| {
        let n = app.shelf(to).len();
        button(text(format!("{label}  {n}")).font(theme::SEMI).size(13))
            .on_press(Message::Nav(Screen::Library(to)))
            .padding([8, 16])
            .style(theme::tab(shelf == to))
    };
    let sort = app.prefs.sort;
    let view = app.prefs.view;
    let sorts = segmented(vec![
        seg_text(
            "Recent",
            sort == Sort::Recent,
            Message::Pref(Pref::Sort(Sort::Recent)),
        ),
        seg_text(
            "A to Z",
            sort == Sort::Title,
            Message::Pref(Pref::Sort(Sort::Title)),
        ),
        seg_text(
            "Most read",
            sort == Sort::Progress,
            Message::Pref(Pref::Sort(Sort::Progress)),
        ),
    ]);
    let views = segmented(vec![
        seg_icon(
            icon::GRID,
            view == View::Covers,
            Message::Pref(Pref::View(View::Covers)),
        ),
        seg_icon(
            icon::LIST,
            view == View::List,
            Message::Pref(Pref::View(View::List)),
        ),
    ]);
    let toolbar = row![
        tab("All", Shelf::All),
        tab("Reading", Shelf::Reading),
        tab("Favorites", Shelf::Favorites),
        space().width(Length::Fill),
        sorts,
        views,
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let body: Element<'_, Message> = if entries.is_empty() {
        container(text(empty).size(15).color(MUTED))
            .padding([40, 0])
            .into()
    } else if view == View::List {
        list_rows(app, entries)
    } else {
        cover_grid(entries.into_iter().map(|e| entry_card(app, e)).collect())
    };
    scroll_page(
        column![
            text("Library").font(theme::BLACK).size(38),
            toolbar,
            space().height(4),
            body
        ]
        .spacing(18),
    )
}

/// A segmented control out of `segments`.
fn segmented<'a>(segments: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    container(Row::with_children(segments).spacing(2))
        .padding(3)
        .style(theme::segments)
        .into()
}

fn seg_text<'a>(label: &'a str, on: bool, msg: Message) -> Element<'a, Message> {
    button(text(label).font(theme::SEMI).size(12))
        .on_press(msg)
        .padding([6, 12])
        .style(theme::segment(on))
        .into()
}

fn seg_icon<'a>(glyph: char, on: bool, msg: Message) -> Element<'a, Message> {
    button(icon(glyph, 15.0))
        .on_press(msg)
        .padding([6, 10])
        .style(theme::segment(on))
        .into()
}

fn list_rows<'a>(app: &'a App, entries: Vec<&'a LibraryEntry>) -> Element<'a, Message> {
    let mut rows = Column::new().spacing(2);
    for e in entries {
        let title = &e.series.title;
        let color = theme::vivid(cover_color(app, title));
        let progress: Element<'a, Message> = match entry_progress(e) {
            Some((done, total)) => container(meter(done, total, color, 4.0)).width(150).into(),
            None => space().width(150).into(),
        };
        let heart: Element<'a, Message> = if app.is_favorite(e.series.id) {
            icon(icon::HEART, 14.0).color(ACCENT).width(18).into()
        } else {
            space().width(18).into()
        };
        let line = row![
            cover(
                app,
                title,
                46.0.into(),
                (46.0 * COVER_RATIO).into(),
                16.0,
                5.0
            ),
            column![
                text(title).font(theme::BOLD).size(16),
                text(entry_status(e)).size(12).color(MUTED),
            ]
            .spacing(3)
            .width(Length::Fill),
            progress,
            text(format!("{} read", e.read))
                .size(12)
                .color(MUTED)
                .width(80),
            text(relative(e.progress.updated_at))
                .size(12)
                .color(FAINT)
                .width(110),
            heart,
        ]
        .spacing(18)
        .align_y(Alignment::Center);
        rows = rows.push(
            button(line)
                .on_press(Message::OpenEntry {
                    id: e.series.id,
                    read: false,
                })
                .width(Length::Fill)
                .padding([8, 12])
                .clip(true)
                .style(theme::row),
        );
    }
    rows.into()
}

fn card_height() -> f32 {
    CARD_W * COVER_RATIO + 78.0
}

fn cover_grid(cards: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    grid(cards)
        .fluid(CARD_W + 16.0)
        .spacing(4)
        .height(grid::aspect_ratio(CARD_W + 16.0, card_height()))
        .into()
}

fn entry_card<'a>(app: &'a App, e: &'a LibraryEntry) -> Element<'a, Message> {
    let favorite = app.is_favorite(e.series.id);
    card_with(
        app,
        &e.series.title,
        entry_status(e),
        entry_progress(e),
        favorite,
        Message::OpenEntry {
            id: e.series.id,
            read: false,
        },
    )
}

/// How far into its chapter a library series is.
fn entry_progress(e: &LibraryEntry) -> Option<(u32, u32)> {
    match (e.progress.read, e.progress.last_page, e.progress.page_count) {
        (false, Some(p), Some(n)) => Some((p + 1, n)),
        _ => None,
    }
}

fn card<'a>(
    app: &'a App,
    title: &'a str,
    note: String,
    progress: Option<(u32, u32)>,
    msg: Message,
) -> Element<'a, Message> {
    card_with(app, title, note, progress, false, msg)
}

/// A cover with the series' name and a line about it; a meter under the cover when a
/// chapter is in progress, and a heart on it for favorites.
fn card_with<'a>(
    app: &'a App,
    title: &'a str,
    note: String,
    progress: Option<(u32, u32)>,
    favorite: bool,
    msg: Message,
) -> Element<'a, Message> {
    let picture = container(cover(app, title, Length::Fill, Length::Fill, 34.0, 8.0))
        .style(theme::shadow(8.0));
    let picture: Element<'a, Message> = if favorite {
        let heart = container(
            container(icon(icon::HEART, 12.0).color(theme::ON_ACCENT))
                .center(24)
                .style(theme::fill(ACCENT, 100.0)),
        )
        .padding(8)
        .width(Length::Fill)
        .align_x(Alignment::End);
        stack![picture, heart].into()
    } else {
        picture.into()
    };
    let mut body = column![picture].spacing(8);
    let line: Element<'a, Message> = match progress {
        Some((done, total)) => meter(done, total, theme::vivid(cover_color(app, title)), 3.0),
        None => space().height(3).into(),
    };
    body = body.push(line).push(
        column![
            text(title)
                .font(theme::BOLD)
                .size(14)
                .wrapping(text::Wrapping::None),
            text(note)
                .size(12)
                .color(MUTED)
                .wrapping(text::Wrapping::None),
        ]
        .spacing(3),
    );
    button(body)
        .on_press(msg)
        .padding(8)
        .width(Length::Fill)
        .height(Length::Fill)
        .clip(true)
        .style(theme::card)
        .into()
}

/// A series' cover, or its initial on its color while there's none.
fn cover<'a>(
    app: &'a App,
    title: &str,
    width: Length,
    height: Length,
    initial: f32,
    radius: f32,
) -> Element<'a, Message> {
    if let Some(handle) = app.cover(title).and_then(|c| c.image.clone()) {
        return image(handle)
            .width(width)
            .height(height)
            .content_fit(ContentFit::Cover)
            .border_radius(radius)
            .into();
    }
    let letter: String = title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    container(
        text(letter)
            .font(theme::BLACK)
            .size(initial)
            .color(theme::alpha(TEXT, 0.35)),
    )
    .center(Length::Fill)
    .width(width)
    .height(height)
    .style(theme::fill(theme::deep(cover_color(app, title)), radius))
    .into()
}

fn cover_color(app: &App, title: &str) -> Option<Color> {
    app.cover(title).and_then(|c| c.color)
}

/// How far along a chapter or series is, as a line.
fn meter<'a>(done: u32, total: u32, color: Color, height: f32) -> Element<'a, Message> {
    let total = total.max(1);
    let done = done.min(total);
    let portion = |n: u32| Length::FillPortion(n.min(u16::MAX as u32) as u16);
    row![
        container(space())
            .width(portion(done))
            .height(height)
            .style(theme::fill(color, height / 2.0)),
        container(space())
            .width(portion(total - done))
            .height(height)
            .style(theme::fill(theme::alpha(TEXT, 0.14), height / 2.0)),
    ]
    .spacing(if done > 0 && done < total { 3 } else { 0 })
    .into()
}

fn entry_status(e: &LibraryEntry) -> String {
    match (e.progress.read, e.progress.last_page, e.progress.page_count) {
        (true, ..) => format!("Read chapter {}", e.chapter),
        (false, Some(p), Some(n)) => format!("Chapter {}, page {} of {n}", e.chapter, p + 1),
        _ => format!("Chapter {}", e.chapter),
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

/// Whether a chapter came out recently enough to call new.
fn is_new(published: Option<jiff::Timestamp>) -> bool {
    let Some(at) = published else { return false };
    jiff::Timestamp::now().as_second() - at.as_second() < 21 * 86_400
}

/// A description cut at a word near `SYNOPSIS` characters, and whether anything was cut.
fn synopsis(description: &str) -> (String, bool) {
    if description.chars().count() <= SYNOPSIS {
        return (description.to_string(), false);
    }
    let cut: String = description.chars().take(SYNOPSIS).collect();
    let end = cut.rfind(' ').unwrap_or(cut.len());
    (
        format!("{}…", cut[..end].trim_end_matches([',', '.', ';'])),
        true,
    )
}

fn pill<'a>(label: String, c: Color, solid: bool) -> Element<'a, Message> {
    container(text(label).font(theme::SEMI).size(12))
        .padding([4, 9])
        .style(theme::badge(c, solid))
        .into()
}

pub fn series<'a>(app: &'a App, view: &'a SeriesView) -> Element<'a, Message> {
    let title = &view.series.title;
    let art = app.cover(title);
    let color = theme::vivid(cover_color(app, title));
    let chapters = view.chapters();

    let mut facts = Row::new().spacing(8).align_y(Alignment::Center);
    if let Some(info) = art.map(|c| &c.info) {
        if let Some(score) = info.score {
            facts = facts.push(
                container(
                    row![
                        icon(icon::STAR, 12.0),
                        text(format!("{score}%")).font(theme::BOLD).size(12)
                    ]
                    .spacing(5)
                    .align_y(Alignment::Center),
                )
                .padding([4, 9])
                .style(theme::badge(ACCENT, true)),
            );
        }
        if let Some(status) = &info.status {
            facts = facts.push(pill(status.clone(), TEXT, false));
        }
        if let Some(year) = info.year {
            facts = facts.push(pill(year.to_string(), TEXT, false));
        }
        for genre in info.genres.iter().take(4) {
            facts = facts.push(pill(genre.clone(), TEXT, false));
        }
    }
    let mut info = column![
        text(title).font(theme::BLACK).size(48).line_height(1.02),
        facts
    ]
    .spacing(14)
    .max_width(820);

    let mut actions = Row::new().spacing(10).align_y(Alignment::Center);
    if let (Some(group), Some(source)) = (&view.group, &view.source) {
        if let Some((i, page)) = view.resume() {
            let ch = &chapters[i];
            let label = match (page, view.progress.get(&ch.number)) {
                (0, Some(p)) if p.read => format!("Read chapter {} again", ch.number),
                (0, _) if i == 0 && view.progress.is_empty() => "Start reading".to_string(),
                (0, _) => format!("Read chapter {}", ch.number),
                (p, _) => format!("Continue chapter {}, page {}", ch.number, p + 1),
            };
            actions = actions.push(
                button(
                    row![
                        icon(icon::PLAY, 15.0),
                        text(label).font(theme::BOLD).size(15)
                    ]
                    .spacing(9)
                    .align_y(Alignment::Center),
                )
                .on_press(Message::Continue)
                .padding([12, 26])
                .style(theme::primary),
            );
        }
        actions = actions.push(favorite_button(app, view.series.id));
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
        .padding([9, 14])
        .style(theme::picker)
        .menu_style(theme::menu);
        actions = actions
            .push(space().width(6))
            .push(text("Source").size(13).color(MUTED))
            .push(picker);
    } else if let Some(e) = &view.failed {
        info = info.push(text(e).size(14).color(MUTED));
    } else {
        info = info.push(text("Finding it on your sources").size(14).color(MUTED));
    }
    info = info.push(actions);

    let back = button(icon(icon::LEFT, 20.0))
        .on_press(Message::Back)
        .padding(9)
        .style(theme::secondary);
    let header = column![
        tip(back, "Back", tooltip::Position::Right),
        space().height(Length::Fill),
        row![
            container(cover(
                app,
                title,
                196.0.into(),
                (196.0 * COVER_RATIO).into(),
                64.0,
                10.0
            ))
            .style(theme::shadow(10.0)),
            info.width(Length::Fill)
        ]
        .spacing(36)
        .align_y(Alignment::End),
    ]
    .height(Length::Fill)
    .into();

    let mut body = Column::new().spacing(34);
    if let Some(description) = art.and_then(|c| c.info.description.as_deref()) {
        let (short, cut) = synopsis(description);
        let shown = if app.more {
            description.to_string()
        } else {
            short
        };
        let mut about = column![
            text(shown)
                .size(15)
                .line_height(1.55)
                .color(theme::mix(TEXT, MUTED, 0.45))
        ]
        .spacing(6)
        .max_width(820);
        if cut {
            about = about.push(
                button(
                    text(if app.more { "Show less" } else { "Show more" })
                        .font(theme::BOLD)
                        .size(13),
                )
                .on_press(Message::More)
                .padding([4, 0])
                .style(theme::ghost),
            );
        }
        body = body.push(about);
    }
    if !chapters.is_empty() {
        body = body.push(chapter_list(app, view, color));
    }
    scrollable(column![
        backdrop(app, title, header),
        container(padded(body)).center_x(Length::Fill)
    ])
    .height(Length::Fill)
    .style(theme::scroll)
    .into()
}

fn chapter_list<'a>(app: &'a App, view: &'a SeriesView, color: Color) -> Element<'a, Message> {
    let Some(source) = &view.source else {
        return space().into();
    };
    let chapters = view.chapters();
    let order: Box<dyn Iterator<Item = usize>> = if app.newest_first {
        Box::new((0..chapters.len()).rev())
    } else {
        Box::new(0..chapters.len())
    };
    let hairline = || {
        container(space())
            .width(Length::Fill)
            .height(1)
            .style(theme::fill(theme::alpha(TEXT, 0.06), 0.0))
    };
    let mut list = Column::new();
    let mut shown = 0;
    for i in order {
        let ch = &chapters[i];
        let progress = view.progress.get(&ch.number);
        let read = progress.is_some_and(|p| p.read);
        if read && app.unread_only {
            continue;
        }
        if shown > 0 {
            list = list.push(hairline());
        }
        shown += 1;
        let in_progress = match progress {
            Some(p) if !p.read && p.source.as_deref() == Some(source.slug()) => {
                match (p.last_page, p.page_count) {
                    (Some(page), Some(count)) => Some((page + 1, count)),
                    _ => None,
                }
            }
            _ => None,
        };
        let date = ch
            .published
            .map(|t| t.strftime("%b %-d, %Y").to_string())
            .unwrap_or_default();
        let ink = if read { FAINT } else { TEXT };
        let mut name = row![
            text(format!("Chapter {}", ch.number))
                .font(theme::BOLD)
                .size(15)
                .color(ink)
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if let Some(title) = ch.title.as_deref().filter(|t| !t.trim().is_empty()) {
            name = name.push(
                text(title.trim().to_string())
                    .size(14)
                    .color(if read { FAINT } else { MUTED })
                    .wrapping(text::Wrapping::None),
            );
        }
        if !read && is_new(ch.published) {
            name = name.push(pill("New".into(), ACCENT, true));
        }
        let mut label = row![container(name).width(Length::Fill)]
            .spacing(16)
            .align_y(Alignment::Center);
        if let Some((page, count)) = in_progress {
            label = label
                .push(container(meter(page, count, color, 4.0)).width(90))
                .push(
                    text(format!("Page {page} of {count}"))
                        .font(theme::SEMI)
                        .size(12)
                        .color(color),
                );
        }
        label = label.push(text(date).size(12).color(FAINT).width(96));
        let page = if read {
            0
        } else {
            resume_page(progress, source)
        };
        let open = button(label)
            .on_press(Message::Read(i, page))
            .width(Length::Fill)
            .padding([13, 12])
            .clip(true)
            .style(theme::row);
        let mark = button(
            icon(
                if read {
                    icon::CIRCLE_CHECK
                } else {
                    icon::CIRCLE
                },
                18.0,
            )
            .color(if read { color } else { FAINT }),
        )
        .on_press(Message::ToggleRead(ch.number))
        .padding(9)
        .style(theme::round_icon(false));
        let mark = tip(
            mark,
            if read {
                "Mark as unread"
            } else {
                "Mark as read"
            },
            tooltip::Position::Left,
        );
        list = list.push(row![mark, open].spacing(4).align_y(Alignment::Center));
    }
    if shown == 0 {
        list = list.push(
            container(
                text("You've read every chapter here.")
                    .size(14)
                    .color(MUTED),
            )
            .padding([20, 12]),
        );
    }
    let read = view.read_count();
    let order = button(
        row![
            icon(icon::SORT, 14.0),
            text(if app.newest_first {
                "Newest first"
            } else {
                "Oldest first"
            })
            .font(theme::SEMI)
            .size(12),
        ]
        .spacing(7)
        .align_y(Alignment::Center),
    )
    .on_press(Message::SortChapters)
    .padding([8, 14])
    .style(theme::tab(false));
    column![
        row![
            column![
                heading(None, "Chapters"),
                text(format!("{read} of {} read", chapters.len()))
                    .size(13)
                    .color(MUTED),
            ]
            .spacing(4)
            .width(Length::Fill),
            container(meter(read as u32, chapters.len() as u32, color, 5.0)).width(180),
            space().width(14),
            segmented(vec![
                seg_text("All", !app.unread_only, Message::UnreadOnly(false)),
                seg_text("Unread", app.unread_only, Message::UnreadOnly(true)),
            ]),
            order,
        ]
        .spacing(10)
        .align_y(Alignment::Center),
        list,
    ]
    .spacing(14)
    .into()
}

pub fn settings<'a>(app: &'a App, ctx: &'a Ctx) -> Element<'a, Message> {
    let r = &app.prefs.reader;
    let choice =
        |label: &'a str, on: bool, change: Pref| seg_text(label, on, Message::Pref(change));
    let setting =
        |name: &'a str, help: &'a str, control: Element<'a, Message>| -> Element<'a, Message> {
            row![
                column![
                    text(name).font(theme::SEMI).size(15),
                    text(help).size(12).color(MUTED)
                ]
                .spacing(4)
                .width(Length::Fill),
                control
            ]
            .spacing(24)
            .align_y(Alignment::Center)
            .padding([14, 0])
            .into()
        };
    let section = |title: &'a str, rows: Vec<Element<'a, Message>>| -> Element<'a, Message> {
        container(
            column![
                text(title).font(theme::HEAVY).size(20),
                Column::with_children(rows)
            ]
            .spacing(6),
        )
        .padding([22, 26])
        .width(Length::Fill)
        .style(theme::fill(theme::SURFACE, 14.0))
        .into()
    };
    let reading = section(
        "Reading",
        vec![
            setting(
                "Show chapters as",
                "A long strip you scroll, or pages you turn.",
                segmented(vec![
                    choice("Strip", r.mode == Mode::Strip, Pref::Mode(Mode::Strip)),
                    choice("Pages", r.mode == Mode::Pages, Pref::Mode(Mode::Pages)),
                ]),
            ),
            setting(
                "Page order",
                "Manga reads right to left; most webtoons and comics left to right.",
                segmented(vec![
                    choice("Right to left", r.right_to_left, Pref::RightToLeft(true)),
                    choice("Left to right", !r.right_to_left, Pref::RightToLeft(false)),
                ]),
            ),
            setting(
                "Two pages side by side",
                "In pages, show facing pages like an open book. The first page stays alone.",
                segmented(vec![
                    choice("On", r.two_pages, Pref::TwoPages(true)),
                    choice("Off", !r.two_pages, Pref::TwoPages(false)),
                ]),
            ),
            setting(
                "Page size",
                "In pages, fit the whole page in the window, or its width and scroll the rest.",
                segmented(vec![
                    choice("Whole page", r.fit == Fit::Page, Pref::Fit(Fit::Page)),
                    choice("Width", r.fit == Fit::Width, Pref::Fit(Fit::Width)),
                ]),
            ),
            setting(
                "Behind the pages",
                "Black hides the window's edges; paper matches pages with white margins.",
                segmented(vec![
                    choice(
                        "Dark",
                        r.backdrop == Backdrop::Ink,
                        Pref::Backdrop(Backdrop::Ink),
                    ),
                    choice(
                        "Black",
                        r.backdrop == Backdrop::Black,
                        Pref::Backdrop(Backdrop::Black),
                    ),
                    choice(
                        "Paper",
                        r.backdrop == Backdrop::Paper,
                        Pref::Backdrop(Backdrop::Paper),
                    ),
                ]),
            ),
        ],
    );
    let library_section = section(
        "Artwork and discovery",
        vec![setting(
            "Use AniList",
            "Covers, banners, descriptions and trending come from anilist.co, looked up by title and kept on disk.",
            segmented(vec![
                choice("On", app.prefs.covers, Pref::Covers(true)),
                choice("Off", !app.prefs.covers, Pref::Covers(false)),
            ]),
        )],
    );
    let data = app
        .data_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let mut names: Vec<String> = ctx
        .sources
        .iter()
        .map(|s| s.id().name().to_string())
        .collect();
    names.sort();
    let tracking = match &ctx.tracking {
        Some(t) => format!("Signed in as {}", t.login.name),
        None => "Not signed in. Run hon login in a terminal.".to_string(),
    };
    let about = section(
        "hon",
        vec![
            setting(
                "Sources",
                "Installed from plugin repos. Manage them with hon repo and hon source.",
                text(names.join(", ")).size(13).color(MUTED).into(),
            ),
            setting(
                "AniList progress",
                "Chapters you finish are sent to your AniList list.",
                text(tracking).size(13).color(MUTED).into(),
            ),
            setting(
                "Your data",
                "Progress is shared with hon in the terminal.",
                text(data).size(12).color(FAINT).into(),
            ),
            setting(
                "Version",
                "hon-gui, from the hon workspace.",
                text(env!("CARGO_PKG_VERSION")).size(13).color(MUTED).into(),
            ),
        ],
    );
    let keys = section(
        "Keyboard",
        vec![container(shortcut_table()).padding([10, 0]).into()],
    );
    scroll_page(
        column![
            text("Settings").font(theme::BLACK).size(38),
            reading,
            library_section,
            keys,
            about,
        ]
        .spacing(18)
        .max_width(920),
    )
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

    #[test]
    fn chapters_out_in_the_last_three_weeks_are_new() {
        let now = jiff::Timestamp::now().as_second();
        let at = |days: i64| jiff::Timestamp::from_second(now - days * 86_400).ok();
        assert!(is_new(at(3)));
        assert!(!is_new(at(30)));
        assert!(!is_new(None));
    }

    #[test]
    fn long_descriptions_are_cut_at_a_word() {
        assert_eq!(synopsis("Short."), ("Short.".to_string(), false));
        let long = "word ".repeat(200);
        let (short, cut) = synopsis(&long);
        assert!(cut);
        assert!(short.ends_with("word…"), "{short}");
        assert!(short.chars().count() <= SYNOPSIS + 1);
    }
}
