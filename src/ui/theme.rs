//! Semantic multi-depth color & style tokens for the Notion Terminal UI.
//!
//! Includes 5 curated truecolor palettes (`Clean Dark`, `Warm Paper`, `Tokyo Night`,
//! `Catppuccin`, `Nord`), plus `ANSI 256`, `ANSI 16`, and `NO_COLOR`, and all 10
//! canonical Notion property/callout colors.

use ratatui::style::{Color, Modifier, Style};

use crate::{config::ThemePreset, models::NotionColor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub no_color: bool,
    pub ansi16_only: bool,
    pub is_light: bool,
    // Surfaces
    pub bg_canvas: Color,
    pub bg_sidebar: Color,
    pub bg_header: Color,
    pub bg_composer: Color,
    pub bg_elevated: Color,
    pub bg_selected: Color,
    pub bg_code: Color,
    pub bg_card: Color,
    // Foregrounds
    pub fg_primary: Color,
    pub fg_secondary: Color,
    pub fg_muted: Color,
    pub fg_subtle: Color,
    // Semantic
    pub accent: Color,
    pub accent_soft: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub code_fg: Color,
    // Shared aliases
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub divider: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::clean_dark()
    }
}

impl Theme {
    pub fn detect(preset: ThemePreset) -> Self {
        if preset == ThemePreset::NoColor
            || std::env::var("NO_COLOR").is_ok_and(|v| !v.trim().is_empty())
        {
            return Self::no_color();
        }
        match preset {
            ThemePreset::CleanDark => Self::clean_dark(),
            ThemePreset::WarmPaper => Self::warm_paper(),
            ThemePreset::TokyoNight => Self::tokyo_night(),
            ThemePreset::Catppuccin => Self::catppuccin(),
            ThemePreset::Nord => Self::nord(),
            ThemePreset::Ansi256 => Self::ansi256(),
            ThemePreset::Ansi16 => Self::ansi16(),
            ThemePreset::NoColor => Self::no_color(),
            ThemePreset::Auto => {
                let colorterm = std::env::var("COLORTERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if colorterm.contains("truecolor") || colorterm.contains("24bit") {
                    return Self::clean_dark();
                }
                let term = std::env::var("TERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if term == "dumb" {
                    return Self::no_color();
                }
                if term.contains("256color") {
                    return Self::ansi256();
                }
                if cfg!(windows) || std::env::var("WT_SESSION").is_ok() {
                    return Self::clean_dark();
                }
                if term.is_empty() {
                    Self::ansi256()
                } else {
                    Self::ansi16()
                }
            }
        }
    }

    /// Notion Clean Dark palette (`#191919` canvas, `#202020` sidebar, `#2F2F2F` card).
    pub fn clean_dark() -> Self {
        let bg_selected = Color::Rgb(47, 52, 61);
        let fg_primary = Color::Rgb(235, 235, 232);
        let fg_muted = Color::Rgb(155, 154, 151);
        let fg_subtle = Color::Rgb(84, 84, 82);
        let accent = Color::Rgb(82, 156, 202);
        let accent_soft = Color::Rgb(126, 189, 228);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(25, 25, 25),
            bg_sidebar: Color::Rgb(32, 32, 32),
            bg_header: Color::Rgb(30, 30, 30),
            bg_composer: Color::Rgb(37, 37, 37),
            bg_elevated: Color::Rgb(40, 40, 40),
            bg_selected,
            bg_code: Color::Rgb(36, 36, 38),
            bg_card: Color::Rgb(37, 37, 37),
            fg_primary,
            fg_secondary: Color::Rgb(203, 202, 198),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            success: Color::Rgb(77, 171, 154),
            warning: Color::Rgb(255, 163, 68),
            error: Color::Rgb(255, 115, 105),
            code_fg: Color::Rgb(235, 87, 87),
            fg: fg_primary,
            muted: fg_muted,
            border: Color::Rgb(55, 55, 55),
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: Color::Rgb(50, 50, 50),
        }
    }

    /// Notion Warm Paper Light palette (`#FFFFFF` canvas, `#F7F6F3` sidebar, `#37352F` ink).
    pub fn warm_paper() -> Self {
        let bg_selected = Color::Rgb(232, 238, 247);
        let fg_primary = Color::Rgb(55, 53, 47);
        let fg_muted = Color::Rgb(120, 119, 116);
        let fg_subtle = Color::Rgb(204, 202, 197);
        let accent = Color::Rgb(35, 131, 226);
        let accent_soft = Color::Rgb(11, 110, 153);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: true,
            bg_canvas: Color::Rgb(255, 255, 255),
            bg_sidebar: Color::Rgb(247, 246, 243),
            bg_header: Color::Rgb(251, 251, 250),
            bg_composer: Color::Rgb(247, 246, 243),
            bg_elevated: Color::Rgb(244, 243, 239),
            bg_selected,
            bg_code: Color::Rgb(241, 241, 239),
            bg_card: Color::Rgb(247, 246, 243),
            fg_primary,
            fg_secondary: Color::Rgb(79, 77, 71),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            success: Color::Rgb(15, 123, 108),
            warning: Color::Rgb(217, 115, 13),
            error: Color::Rgb(224, 62, 62),
            code_fg: Color::Rgb(235, 87, 87),
            fg: fg_primary,
            muted: fg_muted,
            border: Color::Rgb(225, 224, 220),
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: Color::Rgb(233, 233, 231),
        }
    }

    pub fn tokyo_night() -> Self {
        let bg_selected = Color::Rgb(41, 46, 66);
        let fg_primary = Color::Rgb(192, 202, 245);
        let fg_muted = Color::Rgb(115, 122, 162);
        let fg_subtle = Color::Rgb(65, 72, 104);
        let accent = Color::Rgb(122, 162, 247);
        let accent_soft = Color::Rgb(125, 207, 255);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(26, 27, 38),
            bg_sidebar: Color::Rgb(22, 22, 30),
            bg_header: Color::Rgb(31, 35, 53),
            bg_composer: Color::Rgb(36, 40, 59),
            bg_elevated: Color::Rgb(36, 40, 59),
            bg_selected,
            bg_code: Color::Rgb(22, 22, 30),
            bg_card: Color::Rgb(31, 35, 53),
            fg_primary,
            fg_secondary: Color::Rgb(169, 177, 214),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            success: Color::Rgb(115, 218, 202),
            warning: Color::Rgb(224, 175, 104),
            error: Color::Rgb(247, 118, 142),
            code_fg: Color::Rgb(255, 158, 100),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn catppuccin() -> Self {
        let bg_selected = Color::Rgb(49, 50, 68);
        let fg_primary = Color::Rgb(205, 214, 244);
        let fg_muted = Color::Rgb(147, 153, 178);
        let fg_subtle = Color::Rgb(88, 91, 112);
        let accent = Color::Rgb(137, 180, 250);
        let accent_soft = Color::Rgb(148, 226, 213);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(30, 30, 46),
            bg_sidebar: Color::Rgb(24, 24, 37),
            bg_header: Color::Rgb(36, 39, 58),
            bg_composer: Color::Rgb(49, 50, 68),
            bg_elevated: Color::Rgb(49, 50, 68),
            bg_selected,
            bg_code: Color::Rgb(24, 24, 37),
            bg_card: Color::Rgb(38, 39, 56),
            fg_primary,
            fg_secondary: Color::Rgb(186, 194, 222),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            success: Color::Rgb(166, 227, 161),
            warning: Color::Rgb(249, 226, 175),
            error: Color::Rgb(243, 139, 168),
            code_fg: Color::Rgb(250, 179, 135),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn nord() -> Self {
        let bg_selected = Color::Rgb(67, 76, 94);
        let fg_primary = Color::Rgb(236, 239, 244);
        let fg_muted = Color::Rgb(143, 158, 179);
        let fg_subtle = Color::Rgb(76, 86, 106);
        let accent = Color::Rgb(136, 192, 208);
        let accent_soft = Color::Rgb(129, 161, 193);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(46, 52, 64),
            bg_sidebar: Color::Rgb(39, 44, 54),
            bg_header: Color::Rgb(59, 66, 82),
            bg_composer: Color::Rgb(59, 66, 82),
            bg_elevated: Color::Rgb(59, 66, 82),
            bg_selected,
            bg_code: Color::Rgb(39, 44, 54),
            bg_card: Color::Rgb(59, 66, 82),
            fg_primary,
            fg_secondary: Color::Rgb(216, 222, 233),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            success: Color::Rgb(163, 190, 140),
            warning: Color::Rgb(235, 203, 139),
            error: Color::Rgb(191, 97, 106),
            code_fg: Color::Rgb(208, 135, 112),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn ansi256() -> Self {
        let bg_selected = Color::Indexed(237);
        let fg_primary = Color::Indexed(254);
        let fg_muted = Color::Indexed(244);
        let fg_subtle = Color::Indexed(239);
        let accent = Color::Indexed(74);
        let accent_soft = Color::Indexed(117);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Indexed(234),
            bg_header: Color::Indexed(235),
            bg_composer: Color::Indexed(235),
            bg_elevated: Color::Indexed(236),
            bg_selected,
            bg_code: Color::Indexed(235),
            bg_card: Color::Indexed(235),
            fg_primary,
            fg_secondary: Color::Indexed(249),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            success: Color::Indexed(79),
            warning: Color::Indexed(179),
            error: Color::Indexed(204),
            code_fg: Color::Indexed(209),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn ansi16() -> Self {
        Self {
            no_color: false,
            ansi16_only: true,
            is_light: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::DarkGray,
            bg_code: Color::Black,
            bg_card: Color::Reset,
            fg_primary: Color::Reset,
            fg_secondary: Color::White,
            fg_muted: Color::DarkGray,
            fg_subtle: Color::DarkGray,
            accent: Color::Cyan,
            accent_soft: Color::LightCyan,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            code_fg: Color::Yellow,
            fg: Color::Reset,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            border_focus: Color::Cyan,
            selection_bg: Color::DarkGray,
            selection_fg: Color::White,
            divider: Color::DarkGray,
        }
    }

    pub fn no_color() -> Self {
        Self {
            no_color: true,
            ansi16_only: true,
            is_light: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::Reset,
            bg_code: Color::Reset,
            bg_card: Color::Reset,
            fg_primary: Color::Reset,
            fg_secondary: Color::Reset,
            fg_muted: Color::Reset,
            fg_subtle: Color::Reset,
            accent: Color::Reset,
            accent_soft: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            code_fg: Color::Reset,
            fg: Color::Reset,
            muted: Color::Reset,
            border: Color::Reset,
            border_focus: Color::Reset,
            selection_bg: Color::Reset,
            selection_fg: Color::Reset,
            divider: Color::Reset,
        }
    }

    /// Maps Notion's 10 canonical semantic colors to tinted pill `(fg, bg)` styles for Status/Select/Multi-Select tags.
    pub fn notion_pill_style(&self, color: NotionColor) -> Style {
        if self.no_color {
            return Style::default().add_modifier(Modifier::BOLD);
        }
        if self.ansi16_only {
            let fg = match color {
                NotionColor::Gray => Color::Gray,
                NotionColor::Brown | NotionColor::Orange | NotionColor::Yellow => Color::Yellow,
                NotionColor::Green => Color::Green,
                NotionColor::Blue => Color::Cyan,
                NotionColor::Purple | NotionColor::Pink => Color::Magenta,
                NotionColor::Red => Color::Red,
                NotionColor::Default => Color::White,
            };
            return Style::default().fg(fg).add_modifier(Modifier::BOLD);
        }
        if self.is_light {
            let (fg, bg) = match color {
                NotionColor::Gray => (Color::Rgb(80, 78, 74), Color::Rgb(227, 226, 224)),
                NotionColor::Brown => (Color::Rgb(100, 71, 58), Color::Rgb(238, 224, 218)),
                NotionColor::Orange => (Color::Rgb(133, 76, 29), Color::Rgb(250, 222, 201)),
                NotionColor::Yellow => (Color::Rgb(129, 98, 12), Color::Rgb(253, 236, 200)),
                NotionColor::Green => (Color::Rgb(28, 102, 74), Color::Rgb(219, 237, 219)),
                NotionColor::Blue => (Color::Rgb(24, 83, 134), Color::Rgb(211, 229, 239)),
                NotionColor::Purple => (Color::Rgb(73, 47, 100), Color::Rgb(232, 222, 238)),
                NotionColor::Pink => (Color::Rgb(105, 49, 76), Color::Rgb(245, 224, 233)),
                NotionColor::Red => (Color::Rgb(135, 36, 36), Color::Rgb(255, 226, 221)),
                NotionColor::Default => (self.fg_primary, self.bg_elevated),
            };
            return Style::default().fg(fg).bg(bg);
        }
        // Dark palettes: crisp Notion-dark pill tones
        let (fg, bg) = match color {
            NotionColor::Gray => (Color::Rgb(212, 212, 210), Color::Rgb(66, 66, 66)),
            NotionColor::Brown => (Color::Rgb(212, 168, 140), Color::Rgb(74, 52, 42)),
            NotionColor::Orange => (Color::Rgb(255, 179, 102), Color::Rgb(92, 58, 28)),
            NotionColor::Yellow => (Color::Rgb(247, 212, 105), Color::Rgb(86, 70, 28)),
            NotionColor::Green => (Color::Rgb(118, 212, 172), Color::Rgb(35, 74, 59)),
            NotionColor::Blue => (Color::Rgb(126, 192, 240), Color::Rgb(32, 65, 92)),
            NotionColor::Purple => (Color::Rgb(192, 158, 242), Color::Rgb(65, 47, 94)),
            NotionColor::Pink => (Color::Rgb(240, 146, 192), Color::Rgb(89, 43, 66)),
            NotionColor::Red => (Color::Rgb(255, 138, 128), Color::Rgb(94, 41, 38)),
            NotionColor::Default => (self.fg_primary, self.bg_elevated),
        };
        Style::default().fg(fg).bg(bg)
    }

    pub fn notion_text_color(&self, color: NotionColor) -> Color {
        if self.no_color {
            return Color::Reset;
        }
        match color {
            NotionColor::Default => self.fg_primary,
            NotionColor::Gray => self.fg_muted,
            NotionColor::Brown => Color::Rgb(186, 133, 111),
            NotionColor::Orange => Color::Rgb(255, 163, 68),
            NotionColor::Yellow => Color::Rgb(235, 198, 72),
            NotionColor::Green => self.success,
            NotionColor::Blue => self.accent,
            NotionColor::Purple => Color::Rgb(180, 142, 232),
            NotionColor::Pink => Color::Rgb(230, 128, 178),
            NotionColor::Red => self.error,
        }
    }

    pub fn border_style(&self, focused: bool) -> Style {
        if self.no_color {
            return if focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
        }
        Style::default().fg(if focused {
            self.border_focus
        } else {
            self.border
        })
    }

    pub fn selection(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().bg(self.bg_selected)
        }
    }

    pub fn muted_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_muted)
        }
    }

    pub fn subtle_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_subtle)
        }
    }

    pub fn title(&self, focused: bool) -> Style {
        if focused {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            self.muted_style()
        }
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn code_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.code_fg).bg(self.bg_code)
        }
    }

    pub fn quote_style(&self) -> Style {
        Style::default()
            .fg(self.fg_secondary)
            .add_modifier(Modifier::ITALIC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_5_curated_palettes_and_10_notion_colors_render() {
        for preset in [
            ThemePreset::CleanDark,
            ThemePreset::WarmPaper,
            ThemePreset::TokyoNight,
            ThemePreset::Catppuccin,
            ThemePreset::Nord,
            ThemePreset::Ansi256,
            ThemePreset::Ansi16,
            ThemePreset::NoColor,
        ] {
            let t = Theme::detect(preset);
            for c in [
                NotionColor::Default,
                NotionColor::Gray,
                NotionColor::Brown,
                NotionColor::Orange,
                NotionColor::Yellow,
                NotionColor::Green,
                NotionColor::Blue,
                NotionColor::Purple,
                NotionColor::Pink,
                NotionColor::Red,
            ] {
                let _ = t.notion_pill_style(c);
                let _ = t.notion_text_color(c);
            }
        }
    }
}
