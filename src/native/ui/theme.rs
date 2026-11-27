use super::render::{self, Style};
use anyhow::Result;
use crossterm::style::Color;
use serde::{Deserialize, Serialize};
use std::{cell::Cell, fs, io::Read, path::Path};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::native) enum Theme {
    #[default]
    Sage,
    ClaudeInspired,
    Midnight,
    HighContrast,
}

thread_local! {
    static CURRENT: Cell<Theme> = const { Cell::new(Theme::Sage) };
    static OVERLAY: Cell<[Option<Color>; 7]> = const { Cell::new([None; 7]) };
}

const ROLES: [Color; 7] = [
    render::BRAND,
    render::FG,
    render::DIM,
    render::LINE,
    render::CODE,
    render::CODEFG,
    render::WARN,
];

impl Theme {
    pub(in crate::native) const ALL: [Self; 4] = [
        Self::Sage,
        Self::ClaudeInspired,
        Self::Midnight,
        Self::HighContrast,
    ];

    pub(in crate::native) fn label(self) -> &'static str {
        match self {
            Self::Sage => "KarryCode Sage",
            Self::ClaudeInspired => tr!("В стиле Claude Code", "Claude Code Inspired"),
            Self::Midnight => "Midnight",
            Self::HighContrast => tr!("Высокий контраст", "High Contrast"),
        }
    }

    pub(in crate::native) fn load(work: &Path) -> Self {
        fs::File::open(work.join("oc-state/karrycode-agent/appearance.json"))
            .ok()
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(257).read_to_end(&mut bytes).ok()?;
                (bytes.len() <= 256)
                    .then(|| serde_json::from_slice(&bytes).ok())
                    .flatten()
            })
            .unwrap_or_default()
    }

    pub(in crate::native) fn save(self, work: &Path) -> Result<()> {
        let dir = work.join("oc-state/karrycode-agent");
        fs::create_dir_all(&dir)?;
        let mut draft = tempfile::NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(draft.as_file_mut(), &self)?;
        draft.as_file().sync_all()?;
        draft.persist(dir.join("appearance.json"))?;
        Ok(())
    }

    pub(super) fn preview(self, style: Style) -> Style {
        let theme = self;
        if theme == Self::Sage {
            return Style {
                literal_colors: true,
                ..style
            };
        }
        let palette = match theme {
            Self::Sage => unreachable!(),
            Self::ClaudeInspired => [
                0xD97757, 0xEFE8DF, 0xB4A39A, 0x896254, 0x2B2321, 0xE5C4B5, 0xFFBD80,
            ],
            Self::Midnight => [
                0x8FBCFF, 0xE4EBF8, 0xA3B4D2, 0x64799E, 0x202A3D, 0xCCDBF5, 0xFFD897,
            ],
            Self::HighContrast => [
                0xFFFFFF, 0xF2F2F2, 0xC5C5C5, 0x969696, 0x262626, 0xFFFFFF, 0xFFE191,
            ],
        };
        let convert = |color| {
            ROLES
                .iter()
                .position(|&role| role == color)
                .map_or(color, |i| {
                    let rgb = palette[i];
                    Color::Rgb {
                        r: (rgb >> 16) as u8,
                        g: (rgb >> 8) as u8,
                        b: rgb as u8,
                    }
                })
        };
        Style {
            fg: style.fg.map(convert),
            bg: style.bg.map(convert),
            literal_colors: true,
            ..style
        }
    }
}

pub(super) fn logo_colors() -> ([f32; 3], [f32; 3]) {
    let theme = CURRENT.get();
    let rgb = |color| match resolve(Style::fg(color)).fg {
        Some(Color::Rgb { r, g, b }) => [r.into(), g.into(), b.into()],
        _ => unreachable!("logo colors are RGB"),
    };
    let shade = if theme == Theme::Sage && OVERLAY.get()[4].is_none() {
        [30., 40., 34.]
    } else {
        rgb(render::CODE)
    };
    (shade, rgb(render::BRAND))
}

pub(super) fn set(theme: Theme) {
    CURRENT.set(theme);
}

pub(super) fn set_overlay(colors: [Option<Color>; 7]) {
    OVERLAY.set(colors);
}

pub(super) fn resolve(style: Style) -> Style {
    if style.literal_colors {
        style
    } else {
        let builtin = CURRENT.get().preview(style);
        let convert = |original: Option<Color>, fallback: Option<Color>| {
            original
                .and_then(|color| ROLES.iter().position(|role| *role == color))
                .and_then(|index| OVERLAY.get()[index])
                .or(fallback)
        };
        Style {
            fg: convert(style.fg, builtin.fg),
            bg: convert(style.bg, builtin.bg),
            ..builtin
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_round_trips_and_unknown_id_uses_sage() -> Result<()> {
        let work = tempfile::tempdir()?;
        assert_eq!(Theme::load(work.path()), Theme::Sage);
        Theme::ClaudeInspired.save(work.path())?;
        assert_eq!(Theme::load(work.path()), Theme::ClaudeInspired);
        fs::write(
            work.path().join("oc-state/karrycode-agent/appearance.json"),
            b"\"obsolete\"",
        )?;
        assert_eq!(Theme::load(work.path()), Theme::Sage);
        Ok(())
    }
}
