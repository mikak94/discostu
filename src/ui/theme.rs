//! Palette and widget styles. Disco Stu's wardrobe on a dark dance floor:
//! lavender shirt (accent), pink flames, gold medallion, purple-tinted night.

use iced::border::Radius;
use iced::widget::{button, container, pick_list, slider, text_input, toggler};
use iced::{Background, Border, Color, Degrees, Gradient, Shadow, Theme, Vector, color};

pub const BG: Color = color!(0x0d0b12);
pub const SURFACE: Color = color!(0x14121b);
pub const RAISED: Color = color!(0x1c1925);
pub const HOVER: Color = color!(0x262130);
pub const BORDER: Color = color!(0x2b2636);
pub const TEXT: Color = color!(0xeeeaf5);
pub const MUTED: Color = color!(0x948ca6);
pub const FAINT: Color = color!(0x615a72);
/// His shirt.
pub const ACCENT: Color = color!(0xa18cff);
/// The flames on his suit.
pub const PINK: Color = color!(0xf39bc9);
/// The medallion.
pub const GOLD: Color = color!(0xf5c84c);
pub const GREEN: Color = color!(0x3ddc97);
pub const RED: Color = color!(0xff5d6e);
pub const AMBER: Color = color!(0xffb84d);

pub fn theme() -> Theme {
    Theme::custom(
        "discostu",
        iced::theme::Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: GREEN,
            warning: AMBER,
            danger: RED,
        },
    )
}

pub fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

/// Lavender → pink, like the suit.
pub fn disco(angle: f32) -> Background {
    disco_shifted(0.0, angle)
}

fn disco_shifted(amount: f32, angle: f32) -> Background {
    Background::Gradient(Gradient::Linear(
        iced::gradient::Linear::new(Degrees(angle))
            .add_stop(0.0, lighten(ACCENT, amount))
            .add_stop(1.0, lighten(PINK, amount)),
    ))
}

/// Round gradient badge behind Disco Stu's head.
pub fn stu_badge(_: &Theme) -> container::Style {
    container::Style {
        background: Some(disco(135.0)),
        border: Border { radius: 999.0.into(), ..Default::default() },
        shadow: Shadow { color: alpha(PINK, 0.35), offset: Vector::ZERO, blur_radius: 18.0 },
        ..Default::default()
    }
}

/// Channel header row: the one you're in glows a little.
pub fn channel_row(current: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let bg = match (current, status) {
            (true, _) => Some(alpha(ACCENT, 0.14)),
            (false, button::Status::Hovered) => Some(alpha(RAISED, 0.8)),
            (false, button::Status::Pressed) => Some(RAISED),
            _ => None,
        };
        button::Style {
            background: bg.map(Background::Color),
            text_color: if current { TEXT } else { MUTED },
            border: Border {
                color: if current { alpha(ACCENT, 0.35) } else { Color::TRANSPARENT },
                width: 1.0,
                radius: 10.0.into(),
            },
            ..Default::default()
        }
    }
}

const AVATAR_COLORS: [Color; 8] = [
    color!(0xa18cff),
    color!(0x4fc3f7),
    color!(0x3ddc97),
    color!(0xffb84d),
    color!(0xff7eb6),
    color!(0x7ee0d0),
    color!(0xf78c6c),
    color!(0xb4a7ff),
];

pub fn avatar_color(id: u64) -> Color {
    AVATAR_COLORS[(id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 61) as usize]
}

// --- containers -------------------------------------------------------------

pub fn app(_: &Theme) -> container::Style {
    container::Style { background: Some(BG.into()), text_color: Some(TEXT), ..Default::default() }
}

pub fn sidebar(_: &Theme) -> container::Style {
    container::Style {
        background: Some(SURFACE.into()),
        border: Border { color: BORDER, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    }
}

pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(RAISED.into()),
        border: Border { color: BORDER, width: 1.0, radius: 16.0.into() },
        ..Default::default()
    }
}

pub fn tile(speaking: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(SURFACE.into()),
        border: Border {
            color: if speaking { alpha(GREEN, 0.9) } else { BORDER },
            width: if speaking { 2.0 } else { 1.0 },
            radius: 20.0.into(),
        },
        shadow: if speaking {
            Shadow { color: alpha(GREEN, 0.25), offset: Vector::ZERO, blur_radius: 24.0 }
        } else {
            Shadow::default()
        },
        ..Default::default()
    }
}

pub fn dock(_: &Theme) -> container::Style {
    container::Style {
        background: Some(alpha(RAISED, 0.92).into()),
        border: Border { color: BORDER, width: 1.0, radius: 22.0.into() },
        shadow: Shadow { color: alpha(Color::BLACK, 0.45), offset: Vector::new(0.0, 8.0), blur_radius: 28.0 },
        ..Default::default()
    }
}

pub fn pill(bg: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(bg.into()),
        border: Border { radius: 999.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn pill_rect(bg: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(bg.into()),
        border: Border { radius: 10.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn scrim(_: &Theme) -> container::Style {
    container::Style { background: Some(alpha(Color::BLACK, 0.55).into()), ..Default::default() }
}

pub fn modal(_: &Theme) -> container::Style {
    container::Style {
        background: Some(SURFACE.into()),
        border: Border { color: BORDER, width: 1.0, radius: 20.0.into() },
        shadow: Shadow { color: alpha(Color::BLACK, 0.5), offset: Vector::new(0.0, 16.0), blur_radius: 48.0 },
        ..Default::default()
    }
}

pub fn avatar(bg: Color, ring: Option<Color>, size: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(bg.into()),
        text_color: Some(BG),
        border: Border {
            color: ring.unwrap_or(Color::TRANSPARENT),
            width: if ring.is_some() { 2.5 } else { 0.0 },
            radius: (size / 2.0).into(),
        },
        shadow: match ring {
            Some(c) => Shadow { color: alpha(c, 0.45), offset: Vector::ZERO, blur_radius: 14.0 },
            None => Shadow::default(),
        },
        ..Default::default()
    }
}

pub fn meter_track(_: &Theme) -> container::Style {
    container::Style {
        background: Some(HOVER.into()),
        border: Border { radius: 2.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn meter_fill(c: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(c.into()),
        border: Border { radius: 2.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn divider(_: &Theme) -> container::Style {
    container::Style { background: Some(BORDER.into()), ..Default::default() }
}

// --- buttons ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Active,
    Danger,
}

/// Round icon button used in the dock.
pub fn dock_button(tone: Tone) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (base, fg) = match tone {
            Tone::Normal => (HOVER, TEXT),
            Tone::Active => (PINK, BG),
            Tone::Danger => (alpha(RED, 0.18), RED),
        };
        let bg = match status {
            button::Status::Hovered => lighten(base, 0.06),
            button::Status::Pressed => lighten(base, -0.04),
            _ => base,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: fg,
            border: Border { radius: 999.0.into(), ..Default::default() },
            ..Default::default()
        }
    }
}

/// Flat list row (peer list, menus).
pub fn row_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let bg = match (selected, status) {
            (true, _) => Some(RAISED),
            (false, button::Status::Hovered) => Some(alpha(RAISED, 0.7)),
            (false, button::Status::Pressed) => Some(RAISED),
            _ => None,
        };
        button::Style {
            background: bg.map(Background::Color),
            text_color: TEXT,
            border: Border { radius: 12.0.into(), ..Default::default() },
            ..Default::default()
        }
    }
}

pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(Background::Color(HOVER)),
            _ => None,
        },
        text_color: MUTED,
        border: Border { radius: 10.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn primary(_: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => disco_shifted(0.05, 90.0),
        button::Status::Pressed => disco_shifted(-0.05, 90.0),
        button::Status::Disabled => Background::Color(alpha(ACCENT, 0.4)),
        button::Status::Active => disco(90.0),
    };
    button::Style {
        background: Some(bg),
        text_color: color!(0x1a1024),
        border: Border { radius: 10.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn segment(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: Some(Background::Color(match (selected, status) {
            (true, _) => HOVER,
            (false, button::Status::Hovered) => alpha(HOVER, 0.5),
            _ => Color::TRANSPARENT,
        })),
        text_color: if selected { TEXT } else { MUTED },
        border: Border { radius: 8.0.into(), ..Default::default() },
        ..Default::default()
    }
}

fn lighten(c: Color, amount: f32) -> Color {
    let f = |v: f32| (v + amount).clamp(0.0, 1.0);
    Color { r: f(c.r), g: f(c.g), b: f(c.b), a: c.a }
}

// --- inputs -----------------------------------------------------------------

pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(BG),
        border: Border {
            color: match status {
                text_input::Status::Focused { .. } => ACCENT,
                text_input::Status::Hovered => HOVER,
                _ => BORDER,
            },
            width: 1.0,
            radius: 10.0.into(),
        },
        icon: MUTED,
        placeholder: FAINT,
        value: TEXT,
        selection: alpha(ACCENT, 0.35),
    }
}

pub fn picker(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    pick_list::Style {
        text_color: TEXT,
        placeholder_color: FAINT,
        handle_color: MUTED,
        background: Background::Color(BG),
        border: Border {
            color: match status {
                pick_list::Status::Hovered | pick_list::Status::Opened { .. } => HOVER,
                _ => BORDER,
            },
            width: 1.0,
            radius: 10.0.into(),
        },
    }
}

pub fn switch(_: &Theme, status: toggler::Status) -> toggler::Style {
    let on = matches!(status, toggler::Status::Active { is_toggled: true } | toggler::Status::Hovered { is_toggled: true });
    toggler::Style {
        background: Background::Color(if on { ACCENT } else { HOVER }),
        background_border_width: 0.0,
        background_border_color: Color::TRANSPARENT,
        foreground: Background::Color(if on { Color::WHITE } else { MUTED }),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: None,
        border_radius: None,
        padding_ratio: 0.15,
    }
}

pub fn volume(_: &Theme, status: slider::Status) -> slider::Style {
    let handle = if matches!(status, slider::Status::Hovered | slider::Status::Dragged) { Color::WHITE } else { TEXT };
    slider::Style {
        rail: slider::Rail {
            backgrounds: (Background::Color(ACCENT), Background::Color(HOVER)),
            width: 4.0,
            border: Border { radius: Radius::from(2.0), ..Default::default() },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: 7.0 },
            background: Background::Color(handle),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

/// Behind a fullscreen stream: pure black, so letterboxing disappears.
pub fn fullscreen(_: &Theme) -> container::Style {
    container::Style { background: Some(Color::BLACK.into()), text_color: Some(TEXT), ..Default::default() }
}
