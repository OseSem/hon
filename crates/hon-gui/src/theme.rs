//! hon's look: sumi ink and newsprint paper, with each series' cover color as the only
//! other color. Titles are set in Spectral, a book serif; everything else in the system sans.

use iced::border::Radius;
use iced::font::{Family, Weight};
use iced::widget::{button, container, overlay, pick_list, scrollable, slider, text_input};
use iced::{Background, Border, Color, Font, Gradient, Shadow, Theme, Vector, color, gradient};

pub const INK: Color = color!(0x1a1c24);
pub const SURFACE: Color = color!(0x222531);
pub const RAISED: Color = color!(0x2b2f3d);
pub const LINE: Color = color!(0x343849);
pub const PAPER: Color = color!(0xece7dc);
pub const MUTED: Color = color!(0x9a9cab);
pub const FAINT: Color = color!(0x62657a);
pub const DANGER: Color = color!(0xf07a74);

pub const SPECTRAL_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Spectral-SemiBold.ttf");
pub const SPECTRAL_REGULAR: &[u8] = include_bytes!("../assets/fonts/Spectral-Regular.ttf");

pub const DISPLAY: Font = Font {
    family: Family::Name("Spectral"),
    weight: Weight::Semibold,
    ..Font::DEFAULT
};
pub const SERIF: Font = Font {
    family: Family::Name("Spectral"),
    ..Font::DEFAULT
};
pub const STRONG: Font = Font {
    weight: Weight::Semibold,
    ..Font::DEFAULT
};

pub fn theme() -> Theme {
    Theme::custom(
        "hon",
        iced::theme::Palette {
            background: INK,
            text: PAPER,
            primary: PAPER,
            success: color!(0x7fc99a),
            warning: color!(0xe8c170),
            danger: DANGER,
        },
    )
}

/// A series' accent: its cover color, kept light enough to read on ink.
pub fn accent(cover: Option<Color>) -> Color {
    let Some(c) = cover else { return PAPER };
    let lum = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    if lum >= 0.45 {
        return c;
    }
    let t = (0.45 - lum) / (1.0 - lum).max(0.01);
    mix(c, Color::WHITE, t.min(0.8))
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

fn radius(r: f32) -> Border {
    Border {
        radius: Radius::from(r),
        ..Border::default()
    }
}

/// The main action: a slip of paper with ink on it.
pub fn paper(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => color!(0xf7f3ea),
        button::Status::Pressed => color!(0xd8d2c5),
        button::Status::Disabled => RAISED,
        button::Status::Active => PAPER,
    };
    button::Style {
        background: Some(background.into()),
        text_color: if status == button::Status::Disabled {
            FAINT
        } else {
            INK
        },
        border: radius(6.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Secondary actions: text on ink that lifts on hover.
pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    let (background, text_color) = match status {
        button::Status::Hovered => (Some(SURFACE.into()), PAPER),
        button::Status::Pressed => (Some(RAISED.into()), PAPER),
        button::Status::Disabled => (None, FAINT),
        button::Status::Active => (None, MUTED),
    };
    button::Style {
        background,
        text_color,
        border: radius(6.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A toggle in a group: filled when on.
pub fn chip(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        if on {
            let mut style = paper(theme, status);
            style.border = radius(5.0);
            return style;
        }
        let mut style = ghost(theme, status);
        style.border = Border {
            color: LINE,
            width: 1.0,
            radius: Radius::from(5.0),
        };
        style
    }
}

/// A row in a list: invisible until hovered.
pub fn row(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => Some(SURFACE.into()),
        button::Status::Pressed => Some(RAISED.into()),
        _ => None,
    };
    button::Style {
        background,
        text_color: PAPER,
        border: radius(6.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A cover card: the title brightens on hover.
pub fn card(_: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => PAPER,
            _ => color!(0xd6d1c6),
        },
        border: radius(8.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A page thumbnail: outlined in the series' color when it's on screen.
pub fn thumb(on: bool, accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(SURFACE.into()),
            _ => None,
        },
        text_color: if on { PAPER } else { FAINT },
        border: Border {
            color: if on { accent } else { Color::TRANSPARENT },
            width: 2.0,
            radius: Radius::from(4.0),
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
                backgrounds: (accent.into(), RAISED.into()),
                width: 3.0,
                border: radius(2.0),
            },
            handle: slider::Handle {
                shape: slider::HandleShape::Circle { radius: size },
                background: PAPER.into(),
                border_width: 0.0,
                border_color: Color::TRANSPARENT,
            },
        }
    }
}

pub fn search(_: &Theme, status: text_input::Status) -> text_input::Style {
    let border = match status {
        text_input::Status::Focused { .. } => alpha(PAPER, 0.55),
        text_input::Status::Hovered => LINE,
        _ => SURFACE,
    };
    text_input::Style {
        background: SURFACE.into(),
        border: Border {
            color: border,
            width: 1.0,
            radius: Radius::from(8.0),
        },
        icon: MUTED,
        placeholder: FAINT,
        value: PAPER,
        selection: alpha(PAPER, 0.25),
    }
}

pub fn picker(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let border = match status {
        pick_list::Status::Hovered | pick_list::Status::Opened { .. } => FAINT,
        _ => LINE,
    };
    pick_list::Style {
        text_color: PAPER,
        placeholder_color: FAINT,
        handle_color: MUTED,
        background: Background::Color(alpha(SURFACE, 0.7)),
        border: Border {
            color: border,
            width: 1.0,
            radius: Radius::from(6.0),
        },
    }
}

pub fn menu(_: &Theme) -> overlay::menu::Style {
    overlay::menu::Style {
        background: RAISED.into(),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: Radius::from(6.0),
        },
        text_color: PAPER,
        selected_text_color: INK,
        selected_background: PAPER.into(),
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.4),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
    }
}

pub fn scroll(_: &Theme, status: scrollable::Status) -> scrollable::Style {
    let hovered = matches!(
        status,
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered: true,
            ..
        } | scrollable::Status::Dragged { .. }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: (if hovered { MUTED } else { alpha(FAINT, 0.6) }).into(),
            border: radius(4.0),
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: RAISED.into(),
            border: radius(12.0),
            shadow: Shadow::default(),
            icon: PAPER,
        },
    }
}

/// The wash behind a series header: its cover color fading into the ink.
pub fn wash(cover: Option<Color>) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let tint = cover.map(|c| mix(INK, c, 0.42)).unwrap_or(SURFACE);
        let gradient = gradient::Linear::new(std::f32::consts::FRAC_PI_2 * 1.0)
            .add_stop(0.0, tint)
            .add_stop(1.0, INK);
        container::Style {
            background: Some(Background::Gradient(Gradient::Linear(gradient))),
            border: radius(12.0),
            ..container::Style::default()
        }
    }
}

pub fn fill(c: Color, r: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(c.into()),
        border: radius(r),
        ..container::Style::default()
    }
}

/// The bar under the reader and in headers: where reading is in a chapter or series.
pub fn line(c: Color) -> impl Fn(&Theme) -> container::Style {
    fill(c, 2.0)
}

pub fn toast(_: &Theme) -> container::Style {
    container::Style {
        background: Some(RAISED.into()),
        border: Border {
            color: LINE,
            width: 1.0,
            radius: Radius::from(8.0),
        },
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.45),
            offset: Vector::new(0.0, 10.0),
            blur_radius: 30.0,
        },
        text_color: Some(PAPER),
        ..container::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_covers_are_lifted_and_light_ones_kept() {
        let navy = color!(0x10204a);
        let lifted = accent(Some(navy));
        let lum = |c: Color| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
        assert!(lum(lifted) > lum(navy));
        let peach = color!(0xf1c39a);
        assert_eq!(accent(Some(peach)), peach);
        assert_eq!(accent(None), PAPER);
    }
}
