use iced::theme::Palette;
use iced::{Color, Theme};

#[derive(Debug, Clone, Copy, Default)]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
}

pub(super) struct ButtonColors {
    pub on_accent: Color,
    pub elevated: Color,
    pub card: Color,
    pub card_active: Color,
    pub border_subtle: Color,
    pub accent_soft: Color,
    pub text_muted: Color,
}

pub(super) fn button_colors(theme: &Theme) -> ButtonColors {
    if theme.extended_palette().is_dark {
        ButtonColors {
            on_accent: Color::from_rgb8(0x11, 0x18, 0x23),
            elevated: Color::from_rgb8(0x23, 0x2c, 0x38),
            card: Color::from_rgb8(0x24, 0x30, 0x40),
            card_active: Color::from_rgb8(0x30, 0x46, 0x5d),
            border_subtle: Color::from_rgb8(0x39, 0x4a, 0x61),
            accent_soft: Color::from_rgb8(0x2f, 0x3f, 0x66),
            text_muted: Color::from_rgb8(0x74, 0x83, 0x96),
        }
    } else {
        ButtonColors {
            on_accent: Color::from_rgb8(0x10, 0x1a, 0x28),
            elevated: Color::WHITE,
            card: Color::from_rgb8(0xef, 0xf5, 0xfb),
            card_active: Color::from_rgb8(0xd5, 0xe5, 0xf6),
            border_subtle: Color::from_rgb8(0xd3, 0xdf, 0xec),
            accent_soft: Color::from_rgb8(0xdc, 0xe8, 0xfb),
            text_muted: Color::from_rgb8(0x8d, 0x9b, 0xae),
        }
    }
}

pub fn workspace_theme(mode: ThemeMode) -> Theme {
    // Workspace UI's Blue Mist palette, mapped to Iced's semantic colors.
    let palette = match mode {
        ThemeMode::Light => Palette {
            background: Color::from_rgb8(0xe8, 0xf0, 0xf5),
            text: Color::from_rgb8(0x1a, 0x23, 0x30),
            primary: Color::from_rgb8(0x6f, 0x90, 0xd8),
            ..Palette::LIGHT
        },
        ThemeMode::Dark => Palette {
            background: Color::from_rgb8(0x15, 0x18, 0x1d),
            text: Color::from_rgb8(0xf7, 0xfb, 0xff),
            primary: Color::from_rgb8(0x7d, 0x9d, 0xdf),
            ..Palette::DARK
        },
    };

    Theme::custom(
        match mode {
            ThemeMode::Light => "Blue Mist Light",
            ThemeMode::Dark => "Blue Mist Dark",
        }
        .to_owned(),
        Palette {
            danger: Color::from_rgb8(0xd9, 0x2d, 0x20),
            ..palette
        },
    )
}
