//! hon's look: near-black with one loud yellow, artwork doing the rest. Each series brings
//! its own color, taken from its cover, to its page and to the reader. All text is Figtree;
//! icons are Lucide.

use iced::border::Radius;
use iced::font::{Family, Weight};
use iced::widget::{
    Text, button, container, overlay, pick_list, scrollable, slider, text, text_input,
};
use iced::{Background, Border, Color, Font, Gradient, Shadow, Theme, Vector, color, gradient};

pub const BG: Color = color!(0x0d0d10);
pub const DEEP: Color = color!(0x070709);
pub const SURFACE: Color = color!(0x17171c);
pub const RAISED: Color = color!(0x23232a);
pub const LINE: Color = color!(0x2c2c35);
pub const TEXT: Color = color!(0xf5f5f7);
pub const MUTED: Color = color!(0xa6a6b3);
pub const FAINT: Color = color!(0x6c6c79);
pub const ACCENT: Color = color!(0xffd23f);
/// Text and icons on the accent.
pub const ON_ACCENT: Color = color!(0x191300);
pub const DANGER: Color = color!(0xff6b6b);
/// The reader's paper backdrop: a page's own off-white, so its edges vanish.
pub const PAPER: Color = color!(0xf2efe8);

pub const FONTS: [&[u8]; 7] = [
    include_bytes!("../assets/fonts/Figtree-Regular.ttf"),
    include_bytes!("../assets/fonts/Figtree-Medium.ttf"),
    include_bytes!("../assets/fonts/Figtree-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Figtree-Bold.ttf"),
    include_bytes!("../assets/fonts/Figtree-ExtraBold.ttf"),
    include_bytes!("../assets/fonts/Figtree-Black.ttf"),
    include_bytes!("../assets/icons/lucide.ttf"),
];

const fn figtree(weight: Weight) -> Font {
    Font {
        family: Family::Name("Figtree"),
        weight,
        ..Font::DEFAULT
    }
}

pub const BODY: Font = figtree(Weight::Medium);
pub const SEMI: Font = figtree(Weight::Semibold);
pub const BOLD: Font = figtree(Weight::Bold);
pub const HEAVY: Font = figtree(Weight::ExtraBold);
pub const BLACK: Font = figtree(Weight::Black);
pub const ICONS: Font = Font::with_name("lucide");

/// Lucide glyphs.
pub mod icon {
    pub const HOME: char = '\u{e0f8}';
    pub const COMPASS: char = '\u{e09f}';
    pub const LIBRARY: char = '\u{e105}';
    pub const HEART: char = '\u{e0f5}';
    pub const SEARCH: char = '\u{e154}';
    pub const SETTINGS: char = '\u{e157}';
    pub const LEFT: char = '\u{e072}';
    pub const PLAY: char = '\u{e13f}';
    pub const CIRCLE_CHECK: char = '\u{e225}';
    pub const CIRCLE: char = '\u{e07a}';
    pub const STAR: char = '\u{e179}';
    pub const LIST: char = '\u{e10b}';
    pub const GRID: char = '\u{e104}';
    pub const SORT: char = '\u{e380}';
    pub const PANEL: char = '\u{e12d}';
    pub const COLUMNS: char = '\u{e09c}';
    pub const MAXIMIZE: char = '\u{e115}';
    pub const CLOSE: char = '\u{e1b1}';
    pub const KEYBOARD: char = '\u{e283}';
    pub const PLUS: char = '\u{e140}';
    pub const MINUS: char = '\u{e11f}';
    pub const SCROLL: char = '\u{e4d5}';
    pub const PAGE: char = '\u{e37a}';
    pub const WIDTH: char = '\u{e1c5}';
    pub const FLAME: char = '\u{e0d7}';
    pub const BOOK: char = '\u{e063}';
}

pub fn icon<'a>(glyph: char, size: f32) -> Text<'a> {
    text(glyph.to_string())
        .font(ICONS)
        .size(size)
        .line_height(1.0)
}

pub fn theme() -> Theme {
    Theme::custom(
        "hon",
        iced::theme::Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: color!(0x4ade80),
            warning: ACCENT,
            danger: DANGER,
        },
    )
}

pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

pub fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

fn hsl(c: Color) -> (f32, f32, f32) {
    let (max, min) = (c.r.max(c.g).max(c.b), c.r.min(c.g).min(c.b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-5 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-5);
    let h = if max == c.r {
        ((c.g - c.b) / d).rem_euclid(6.0)
    } else if max == c.g {
        (c.b - c.r) / d + 2.0
    } else {
        (c.r - c.g) / d + 4.0
    };
    (h * 60.0, s.min(1.0), l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color::from_rgb(r + m, g + m, b + m)
}

/// A series' color for marks on the dark background: its cover's hue, bright and saturated.
/// The accent when the cover has no color to speak of.
pub fn vivid(cover: Option<Color>) -> Color {
    match cover.map(hsl) {
        Some((h, s, _)) if s > 0.12 => from_hsl(h, (s * 1.25).clamp(0.55, 0.9), 0.66),
        _ => ACCENT,
    }
}

/// A series' color as a backdrop: the same hue, deep enough for white text.
pub fn deep(cover: Option<Color>) -> Color {
    match cover.map(hsl) {
        Some((h, s, _)) if s > 0.12 => from_hsl(h, (s * 1.1).clamp(0.35, 0.7), 0.24),
        _ => color!(0x2a2a33),
    }
}

fn round(r: f32) -> Border {
    Border {
        radius: Radius::from(r),
        ..Border::default()
    }
}

fn outline(c: Color, r: f32) -> Border {
    Border {
        color: c,
        width: 1.0,
        radius: Radius::from(r),
    }
}

fn plain(background: Option<Color>, text_color: Color, border: Border) -> button::Style {
    button::Style {
        background: background.map(Background::Color),
        text_color,
        border,
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The one thing to do on a screen: a yellow pill.
pub fn primary(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => color!(0xffdf6e),
        button::Status::Pressed => color!(0xe6b800),
        button::Status::Disabled => RAISED,
        button::Status::Active => ACCENT,
    };
    let ink = if status == button::Status::Disabled {
        FAINT
    } else {
        ON_ACCENT
    };
    plain(Some(background), ink, round(100.0))
}

/// Other actions beside it: a frosted pill.
pub fn secondary(_: &Theme, status: button::Status) -> button::Style {
    let a = match status {
        button::Status::Hovered => 0.2,
        button::Status::Pressed => 0.26,
        _ => 0.12,
    };
    plain(Some(alpha(TEXT, a)), TEXT, round(100.0))
}

/// Quiet actions: text that lights up on hover.
pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Hovered => plain(Some(alpha(TEXT, 0.08)), TEXT, round(8.0)),
        button::Status::Pressed => plain(Some(alpha(TEXT, 0.14)), TEXT, round(8.0)),
        button::Status::Disabled => plain(None, alpha(FAINT, 0.6), round(8.0)),
        button::Status::Active => plain(None, MUTED, round(8.0)),
    }
}

/// A round icon button; `on` fills it with the accent.
pub fn round_icon(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        if on {
            return plain(Some(ACCENT), ON_ACCENT, round(100.0));
        }
        match status {
            button::Status::Hovered => plain(Some(alpha(TEXT, 0.12)), TEXT, round(100.0)),
            button::Status::Pressed => plain(Some(alpha(TEXT, 0.18)), TEXT, round(100.0)),
            button::Status::Disabled => plain(None, alpha(FAINT, 0.5), round(100.0)),
            button::Status::Active => plain(None, MUTED, round(100.0)),
        }
    }
}

/// One choice in a segmented control.
pub fn segment(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| match (on, status) {
        (true, _) => plain(Some(alpha(TEXT, 0.16)), TEXT, round(7.0)),
        (false, button::Status::Hovered | button::Status::Pressed) => {
            plain(Some(alpha(TEXT, 0.06)), TEXT, round(7.0))
        }
        _ => plain(None, MUTED, round(7.0)),
    }
}

/// The track a segmented control sits in.
pub fn segments(_: &Theme) -> container::Style {
    container::Style {
        background: Some(alpha(TEXT, 0.06).into()),
        border: round(9.0),
        ..container::Style::default()
    }
}

/// A filter tab: a pill, white when chosen.
pub fn tab(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| match (on, status) {
        (true, _) => plain(Some(TEXT), BG, round(100.0)),
        (false, button::Status::Hovered | button::Status::Pressed) => {
            plain(Some(alpha(TEXT, 0.16)), TEXT, round(100.0))
        }
        _ => plain(Some(alpha(TEXT, 0.08)), TEXT, round(100.0)),
    }
}

/// A sidebar entry.
pub fn nav(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| match (active, status) {
        (true, _) => plain(Some(alpha(TEXT, 0.1)), TEXT, round(8.0)),
        (false, button::Status::Hovered | button::Status::Pressed) => {
            plain(Some(alpha(TEXT, 0.05)), TEXT, round(8.0))
        }
        _ => plain(None, MUTED, round(8.0)),
    }
}

/// A row in a list: invisible until hovered.
pub fn row(_: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Hovered => plain(Some(alpha(TEXT, 0.06)), TEXT, round(8.0)),
        button::Status::Pressed => plain(Some(alpha(TEXT, 0.1)), TEXT, round(8.0)),
        _ => plain(None, TEXT, round(8.0)),
    }
}

/// A cover card: a plate appears under it on hover.
pub fn card(_: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Hovered | button::Status::Pressed => {
            plain(Some(alpha(TEXT, 0.07)), TEXT, round(12.0))
        }
        _ => plain(None, TEXT, round(12.0)),
    }
}

/// A wide card on a surface, as the continue-reading cards are.
pub fn tile(_: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Hovered | button::Status::Pressed => plain(Some(RAISED), TEXT, round(12.0)),
        _ => plain(Some(SURFACE), TEXT, round(12.0)),
    }
}

/// A page thumbnail: outlined in the series' color when it's on screen.
pub fn thumb(on: bool, accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(alpha(TEXT, 0.08).into()),
            _ => None,
        },
        text_color: if on { TEXT } else { FAINT },
        border: Border {
            color: if on { accent } else { Color::TRANSPARENT },
            width: 2.0,
            radius: Radius::from(6.0),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The page slider: the read part in the series' color.
pub fn seek(accent: Color) -> impl Fn(&Theme, slider::Status) -> slider::Style {
    move |_, status| {
        let size = if status == slider::Status::Active {
            6.0
        } else {
            8.0
        };
        slider::Style {
            rail: slider::Rail {
                backgrounds: (accent.into(), alpha(TEXT, 0.14).into()),
                width: 4.0,
                border: round(2.0),
            },
            handle: slider::Handle {
                shape: slider::HandleShape::Circle { radius: size },
                background: TEXT.into(),
                border_width: 0.0,
                border_color: Color::TRANSPARENT,
            },
        }
    }
}

pub fn search(_: &Theme, status: text_input::Status) -> text_input::Style {
    let (background, border) = match status {
        text_input::Status::Focused { .. } => (alpha(TEXT, 0.12), alpha(TEXT, 0.5)),
        text_input::Status::Hovered => (alpha(TEXT, 0.12), Color::TRANSPARENT),
        _ => (alpha(TEXT, 0.08), Color::TRANSPARENT),
    };
    text_input::Style {
        background: background.into(),
        border: outline(border, 100.0),
        icon: MUTED,
        placeholder: FAINT,
        value: TEXT,
        selection: alpha(ACCENT, 0.4),
    }
}

pub fn picker(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let a = match status {
        pick_list::Status::Hovered | pick_list::Status::Opened { .. } => 0.16,
        _ => 0.1,
    };
    pick_list::Style {
        text_color: TEXT,
        placeholder_color: FAINT,
        handle_color: MUTED,
        background: alpha(TEXT, a).into(),
        border: round(8.0),
    }
}

pub fn menu(_: &Theme) -> overlay::menu::Style {
    overlay::menu::Style {
        background: RAISED.into(),
        border: outline(LINE, 10.0),
        text_color: TEXT,
        selected_text_color: ON_ACCENT,
        selected_background: ACCENT.into(),
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.5),
            offset: Vector::new(0.0, 10.0),
            blur_radius: 30.0,
        },
    }
}

pub fn scroll(_: &Theme, status: scrollable::Status) -> scrollable::Style {
    let hovered = matches!(
        status,
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered: true,
            ..
        } | scrollable::Status::Hovered {
            is_horizontal_scrollbar_hovered: true,
            ..
        } | scrollable::Status::Dragged { .. }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: alpha(TEXT, if hovered { 0.4 } else { 0.16 }).into(),
            border: round(4.0),
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: RAISED.into(),
            border: round(12.0),
            shadow: Shadow::default(),
            icon: TEXT,
        },
    }
}

pub fn fill(c: Color, r: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(c.into()),
        border: round(r),
        ..container::Style::default()
    }
}

pub fn sidebar(_: &Theme) -> container::Style {
    container::Style {
        background: Some(DEEP.into()),
        ..container::Style::default()
    }
}

/// A series' color as a backdrop, for when there's no artwork to show.
pub fn wash(cover: Option<Color>) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let c = deep(cover);
        let gradient = gradient::Linear::new(std::f32::consts::FRAC_PI_2 * 0.6)
            .add_stop(0.0, mix(c, Color::BLACK, 0.2))
            .add_stop(1.0, mix(c, Color::WHITE, 0.12));
        container::Style {
            background: Some(Background::Gradient(Gradient::Linear(gradient))),
            ..container::Style::default()
        }
    }
}

/// Fades artwork into the background: clear at the top, solid at the bottom.
pub fn fade_down(_: &Theme) -> container::Style {
    let gradient = gradient::Linear::new(std::f32::consts::PI)
        .add_stop(0.0, alpha(BG, 0.25))
        .add_stop(0.45, alpha(BG, 0.55))
        .add_stop(0.85, alpha(BG, 0.94))
        .add_stop(1.0, BG);
    container::Style {
        background: Some(Background::Gradient(Gradient::Linear(gradient))),
        ..container::Style::default()
    }
}

/// Darkens artwork's left side, where the title sits.
pub fn fade_side(_: &Theme) -> container::Style {
    let gradient = gradient::Linear::new(std::f32::consts::FRAC_PI_2)
        .add_stop(0.0, alpha(BG, 0.7))
        .add_stop(0.55, alpha(BG, 0.25))
        .add_stop(1.0, alpha(BG, 0.0));
    container::Style {
        background: Some(Background::Gradient(Gradient::Linear(gradient))),
        ..container::Style::default()
    }
}

/// Lifts a cover off the page.
pub fn shadow(r: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        border: round(r),
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.6),
            offset: Vector::new(0.0, 12.0),
            blur_radius: 32.0,
        },
        ..container::Style::default()
    }
}

/// A small label in its color; `solid` fills it.
pub fn badge(c: Color, solid: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(if solid { c } else { alpha(c, 0.14) }.into()),
        border: round(6.0),
        text_color: Some(if solid { ON_ACCENT } else { c }),
        ..container::Style::default()
    }
}

/// Floating panels: notes and the shortcuts sheet.
pub fn floating(_: &Theme) -> container::Style {
    container::Style {
        background: Some(RAISED.into()),
        border: outline(LINE, 12.0),
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.5),
            offset: Vector::new(0.0, 12.0),
            blur_radius: 36.0,
        },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

pub fn tooltip(_: &Theme) -> container::Style {
    container::Style {
        background: Some(color!(0x34343d).into()),
        border: round(6.0),
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(c: Color) -> f32 {
        0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
    }

    #[test]
    fn hsl_round_trips() {
        for c in [
            color!(0x10204a),
            color!(0xf1c39a),
            color!(0x35c46b),
            color!(0x808080),
        ] {
            let (h, s, l) = hsl(c);
            let back = from_hsl(h, s, l);
            assert!((back.r - c.r).abs() < 0.01, "{c:?}");
            assert!((back.g - c.g).abs() < 0.01, "{c:?}");
            assert!((back.b - c.b).abs() < 0.01, "{c:?}");
        }
    }

    #[test]
    fn a_series_color_is_bright_for_marks_and_deep_for_backdrops() {
        let navy = Some(color!(0x10204a));
        assert!(lum(vivid(navy)) > 0.4, "readable on the dark background");
        assert!(lum(deep(navy)) < 0.3, "white text reads on it");
        let gray = Some(color!(0x8a8a8a));
        assert_eq!(vivid(gray), ACCENT, "no hue to take");
        assert_eq!(vivid(None), ACCENT);
    }
}
