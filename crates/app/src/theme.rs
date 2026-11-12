//! Monochrome styling: black on white or white on black, 1px borders, square
//! corners. The only accent is the red "live" marker.

use crate::settings::ThemeChoice;
use iced::theme::Palette;
use iced::widget::slider as sl;
use iced::widget::{button, checkbox, container, text_input};
use iced::{Background, Border, Color, Shadow, Theme};

pub const LIVE: Color = Color::from_rgb(0.9, 0.12, 0.12);

pub fn theme(choice: ThemeChoice) -> Theme {
    let (background, text) = match choice {
        ThemeChoice::Light => (Color::WHITE, Color::BLACK),
        ThemeChoice::Dark => (Color::BLACK, Color::WHITE),
    };
    let name = match choice {
        ThemeChoice::Light => "Mono Light",
        ThemeChoice::Dark => "Mono Dark",
    };
    Theme::custom(
        name,
        Palette {
            background,
            text,
            primary: text,
            success: text,
            warning: text,
            danger: LIVE,
        },
    )
}

fn fg(theme: &Theme) -> Color {
    theme.palette().text
}

fn bg(theme: &Theme) -> Color {
    theme.palette().background
}

fn faded(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

fn border(color: Color) -> Border {
    Border {
        color,
        width: 1.0,
        radius: 0.0.into(),
    }
}

/// Filled button: inverted colors.
pub fn primary(theme: &Theme, status: button::Status) -> button::Style {
    let (fg, bg) = (fg(theme), bg(theme));
    let fill = match status {
        button::Status::Active => fg,
        button::Status::Hovered => faded(fg, 0.85),
        button::Status::Pressed => faded(fg, 0.7),
        button::Status::Disabled => faded(fg, 0.3),
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: bg,
        border: border(fill),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Outlined button.
pub fn secondary(theme: &Theme, status: button::Status) -> button::Style {
    let fg = fg(theme);
    let (fill, text, line) = match status {
        button::Status::Active => (Color::TRANSPARENT, fg, fg),
        button::Status::Hovered => (faded(fg, 0.08), fg, fg),
        button::Status::Pressed => (faded(fg, 0.16), fg, fg),
        button::Status::Disabled => (Color::TRANSPARENT, faded(fg, 0.35), faded(fg, 0.35)),
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: text,
        border: border(line),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Borderless text button.
pub fn ghost(theme: &Theme, status: button::Status) -> button::Style {
    let fg = fg(theme);
    let (fill, text) = match status {
        button::Status::Active => (Color::TRANSPARENT, fg),
        button::Status::Hovered => (faded(fg, 0.08), fg),
        button::Status::Pressed => (faded(fg, 0.16), fg),
        button::Status::Disabled => (Color::TRANSPARENT, faded(fg, 0.35)),
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: text,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Segmented-control option; `selected` renders filled.
pub fn choice(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        if selected {
            primary(theme, status)
        } else {
            secondary(theme, status)
        }
    }
}

pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let fg = fg(theme);
    let line = match status {
        text_input::Status::Focused { .. } => fg,
        text_input::Status::Hovered => faded(fg, 0.7),
        text_input::Status::Active => faded(fg, 0.45),
        text_input::Status::Disabled => faded(fg, 0.2),
    };
    text_input::Style {
        background: Background::Color(bg(theme)),
        border: border(line),
        icon: fg,
        placeholder: faded(fg, 0.4),
        value: fg,
        selection: faded(fg, 0.25),
    }
}

pub fn slider(theme: &Theme, status: sl::Status) -> sl::Style {
    let fg = fg(theme);
    let handle = match status {
        sl::Status::Active => fg,
        _ => faded(fg, 0.8),
    };
    sl::Style {
        rail: sl::Rail {
            backgrounds: (Background::Color(fg), Background::Color(faded(fg, 0.25))),
            width: 2.0,
            border: Border::default(),
        },
        handle: sl::Handle {
            shape: sl::HandleShape::Rectangle {
                width: 6,
                border_radius: 0.0.into(),
            },
            background: Background::Color(handle),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

pub fn check(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let fg = fg(theme);
    let checked = match status {
        checkbox::Status::Active { is_checked }
        | checkbox::Status::Hovered { is_checked }
        | checkbox::Status::Disabled { is_checked } => is_checked,
    };
    checkbox::Style {
        background: Background::Color(if checked { fg } else { bg(theme) }),
        icon_color: bg(theme),
        border: border(fg),
        text_color: Some(fg),
    }
}

/// A bordered panel (tiles, dialogs).
pub fn panel(theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(fg(theme)),
        background: Some(Background::Color(bg(theme))),
        border: border(fg(theme)),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Background behind video; letterbox bars show this.
pub fn video_area(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(faded(fg(theme), 0.06))),
        ..container::Style::default()
    }
}

/// Dims the screen behind a modal.
pub fn scrim(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(faded(bg(theme), 0.75))),
        ..container::Style::default()
    }
}

pub fn muted_text(theme: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(faded(fg(theme), 0.55)),
    }
}

pub fn live_text(_theme: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style { color: Some(LIVE) }
}
