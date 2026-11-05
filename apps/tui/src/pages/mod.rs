//! Page implementations (empty shells in R-11).

mod frame;
mod probe;

pub use frame::PageShell;
pub use probe::{ProbeOutcome, apply_probe_to_shell, read_path};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::state::PageId;
use crate::theme::Theme;

/// Renders and handles keys for one screen.
pub trait Page: Send {
    /// Which tab this page is.
    fn id(&self) -> PageId;

    /// Starts or restarts the page fetch for `generation`.
    fn reload(&mut self, generation: u64);

    /// Handles a key while this page is open in list mode.
    fn on_key(&mut self, key: crossterm::event::KeyEvent) -> PageKeyOutcome;

    /// Draws the page body (not header/footer).
    fn draw_body(&self, area: Rect, buf: &mut Buffer, theme: &Theme);
}

/// Result of a page key handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PageKeyOutcome {
    /// Nothing special.
    #[default]
    None,
    /// Move cursor up (for tests).
    CursorUp,
    /// Move cursor down.
    CursorDown,
}

/// Box of all six pages.
pub struct Pages {
    agents: PageShell,
    providers: PageShell,
    routing: PageShell,
    usage: PageShell,
    sessions: PageShell,
    library: PageShell,
}

impl Default for Pages {
    fn default() -> Self {
        Self {
            agents: PageShell::new(PageId::Agents, "No agents loaded yet."),
            providers: PageShell::new(PageId::Providers, "No providers loaded yet."),
            routing: PageShell::new(PageId::Routing, "No routing groups loaded yet."),
            usage: PageShell::new(PageId::Usage, "No usage loaded yet."),
            sessions: PageShell::new(PageId::Sessions, "Loading sessions…"),
            library: PageShell::new(PageId::Library, "Loading library…"),
        }
    }
}

impl Pages {
    /// Mutable page by id.
    pub fn mut_page(&mut self, id: PageId) -> &mut PageShell {
        match id {
            PageId::Agents => &mut self.agents,
            PageId::Providers => &mut self.providers,
            PageId::Routing => &mut self.routing,
            PageId::Usage => &mut self.usage,
            PageId::Sessions => &mut self.sessions,
            PageId::Library => &mut self.library,
        }
    }

    /// Immutable page by id.
    #[must_use]
    pub fn page(&self, id: PageId) -> &PageShell {
        match id {
            PageId::Agents => &self.agents,
            PageId::Providers => &self.providers,
            PageId::Routing => &self.routing,
            PageId::Usage => &self.usage,
            PageId::Sessions => &self.sessions,
            PageId::Library => &self.library,
        }
    }

    /// Applies an HTTP probe result for deferred routes.
    pub fn apply_probe(&mut self, id: PageId, outcome: ProbeOutcome) {
        apply_probe_to_shell(self.mut_page(id), outcome);
    }
}
