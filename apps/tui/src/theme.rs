//! Adaptive palette and `NO_COLOR` handling.

#![allow(missing_docs)] // Color helpers mirror the reference palette; names are self-explanatory.

use ratatui::style::{Color, Modifier, Style};

/// Whether to paint with the adaptive palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    color: bool,
}

impl Theme {
    /// Builds a theme from CLI and environment.
    #[must_use]
    pub fn new(no_color: bool) -> Self {
        let env = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self {
            color: !(no_color || env),
        }
    }

    /// Forces monochrome for tests.
    #[must_use]
    pub const fn monochrome() -> Self {
        Self { color: false }
    }

    /// Accent (indigo).
    #[must_use]
    pub fn accent(&self) -> Color {
        if self.color {
            Color::Rgb(165, 180, 252)
        } else {
            Color::Reset
        }
    }

    #[must_use]
    pub fn text(&self) -> Color {
        if self.color {
            Color::Rgb(230, 230, 230)
        } else {
            Color::Reset
        }
    }

    #[must_use]
    pub fn muted(&self) -> Color {
        if self.color {
            Color::Rgb(124, 130, 144)
        } else {
            Color::Reset
        }
    }

    #[must_use]
    pub fn ok(&self) -> Color {
        if self.color {
            Color::Rgb(126, 226, 168)
        } else {
            Color::Reset
        }
    }

    #[must_use]
    pub fn bad(&self) -> Color {
        if self.color {
            Color::Rgb(255, 138, 128)
        } else {
            Color::Reset
        }
    }

    #[must_use]
    pub fn pill_bg(&self) -> Color {
        if self.color {
            Color::Rgb(44, 46, 74)
        } else {
            Color::Reset
        }
    }

    #[must_use]
    pub fn title_style(&self) -> Style {
        Style::default()
            .fg(self.accent())
            .add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn pill_style(&self) -> Style {
        Style::default().fg(self.accent()).bg(self.pill_bg())
    }

    #[must_use]
    pub fn cursor_style(&self) -> Style {
        Style::default()
            .fg(self.accent())
            .add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn text_style(&self) -> Style {
        Style::default().fg(self.text())
    }

    #[must_use]
    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted())
    }

    /// Status prefix that remains readable without color.
    #[must_use]
    pub fn status_prefix(&self, ok: bool) -> (&'static str, Style) {
        if ok {
            ("+ ok ", Style::default().fg(self.ok()))
        } else {
            ("! failed ", Style::default().fg(self.bad()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Theme;

    #[test]
    fn no_color_uses_reset_only() {
        let theme = Theme::new(true);
        assert_eq!(theme.accent(), ratatui::style::Color::Reset);
        assert_eq!(theme.text(), ratatui::style::Color::Reset);
    }

    #[test]
    fn status_prefix_uses_words_not_color_alone() {
        let theme = Theme::monochrome();
        let (ok, _) = theme.status_prefix(true);
        let (bad, _) = theme.status_prefix(false);
        assert!(ok.contains("ok"));
        assert!(bad.contains("failed"));
    }
}
