use iced::theme::Palette;
use iced::widget::{button, container};
use iced::{Background, Border, Color, Theme};

/// Static Wallora color palette defined in the baseline specification.
pub mod palette {
    use iced::Color;

    pub const CANVAS: Color = Color::from_rgb(
        0x1E as f32 / 255.0,
        0x1C as f32 / 255.0,
        0x1A as f32 / 255.0,
    );
    pub const SURFACE: Color = Color::from_rgb(
        0x34 as f32 / 255.0,
        0x30 as f32 / 255.0,
        0x2D as f32 / 255.0,
    );
    pub const MUTED: Color = Color::from_rgb(
        0x5E as f32 / 255.0,
        0x5A as f32 / 255.0,
        0x57 as f32 / 255.0,
    );
    pub const TEXT_SECONDARY: Color = Color::from_rgb(
        0xB0 as f32 / 255.0,
        0xAC as f32 / 255.0,
        0xA9 as f32 / 255.0,
    );
    pub const TEXT_PRIMARY: Color = Color::from_rgb(
        0xF5 as f32 / 255.0,
        0xF4 as f32 / 255.0,
        0xF2 as f32 / 255.0,
    );
    pub const ACCENT: Color = Color::from_rgb(
        0xF3 as f32 / 255.0,
        0x73 as f32 / 255.0,
        0x16 as f32 / 255.0,
    );
    pub const WARNING: Color = Color::from_rgb(
        0xF7 as f32 / 255.0,
        0xB1 as f32 / 255.0,
        0x20 as f32 / 255.0,
    );
    pub const DANGER: Color = Color::from_rgb(
        0xE8 as f32 / 255.0,
        0x4D as f32 / 255.0,
        0x23 as f32 / 255.0,
    );
    pub const SUCCESS: Color = Color::from_rgb(
        0x6B as f32 / 255.0,
        0xB3 as f32 / 255.0,
        0x5E as f32 / 255.0,
    );
}

pub fn wallora_theme() -> Theme {
    Theme::custom(
        "Wallora Dark".to_string(),
        Palette {
            background: palette::CANVAS,
            text: palette::TEXT_PRIMARY,
            primary: palette::ACCENT,
            success: palette::SUCCESS,
            warning: palette::WARNING,
            danger: palette::DANGER,
        },
    )
}

pub fn surface_color() -> Color {
    palette::SURFACE
}

pub fn pill_button_style(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let base = if active {
            button::primary(theme, status)
        } else {
            button::secondary(theme, status)
        };
        button::Style {
            background: Some(Background::Color(if active {
                palette::ACCENT
            } else {
                surface_color()
            })),
            text_color: if active {
                palette::CANVAS
            } else {
                palette::TEXT_PRIMARY
            },
            border: Border {
                radius: 6.0.into(),
                width: 1.0,
                color: if active {
                    palette::ACCENT
                } else {
                    palette::MUTED
                },
            },
            ..base
        }
    }
}

pub fn card_style(border_color: Color, border_width: f32) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(surface_color())),
        border: Border {
            radius: 8.0.into(),
            width: border_width,
            color: border_color,
        },
        ..Default::default()
    }
}
