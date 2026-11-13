//! Page implementations.

mod accounts;
mod agents;
mod frame;
mod probe;
mod providers;
pub mod sessions;
mod status;
pub mod usage;

pub use accounts::{AccountsAction, AccountsPage};
pub use agents::AgentPage;
pub use frame::PageShell;
pub use probe::{ProbeOutcome, apply_probe_to_shell, read_path};
pub use providers::ProviderPage;
pub use sessions::SessionsPage;
pub use usage::{UsageAction, UsagePage};

use hofund_control_proto::SessionResume;
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

    /// Last fetch generation recorded.
    fn fetch_gen(&self) -> u64;
}

/// Result of a page key handler.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum PageKeyOutcome {
    /// Nothing special.
    #[default]
    None,
    /// Move cursor up (for tests).
    CursorUp,
    /// Move cursor down.
    CursorDown,
    /// Reload list and stats from control.
    Reload,
    /// Run resume argv in this terminal.
    Resume(SessionResume),
    /// Show cannot resume flash.
    FlashResumeUnavailable,
    /// User cancelled the filter picker.
    ClosePicker,
    /// User picked a model or folder filter.
    PickFilter(String),
    /// Usage page action.
    Usage(UsageAction),
}

/// Box of all pages.
pub struct Pages {
    agents: PageShell,
    providers: PageShell,
    routing: PageShell,
    usage: UsagePage,
    sessions: SessionsPage,
    library: PageShell,
    accounts: AccountsPage,
}

impl Default for Pages {
    fn default() -> Self {
        Self {
            agents: PageShell::new(PageId::Agents, "No agents loaded yet."),
            providers: PageShell::new(PageId::Providers, "No providers loaded yet."),
            routing: PageShell::new(PageId::Routing, "No routing groups loaded yet."),
            usage: UsagePage::new(),
            sessions: SessionsPage::new(),
            library: PageShell::new(PageId::Library, "Loading library…"),
            accounts: AccountsPage::new(),
        }
    }
}

impl Pages {
    /// Mutable list shell when the page is still a shell.
    pub fn mut_shell(&mut self, id: PageId) -> Option<&mut PageShell> {
        match id {
            PageId::Agents => Some(&mut self.agents),
            PageId::Providers => Some(&mut self.providers),
            PageId::Routing => Some(&mut self.routing),
            PageId::Library => Some(&mut self.library),
            PageId::Sessions | PageId::Usage | PageId::Accounts => None,
        }
    }

    /// Mutable sessions page.
    pub fn sessions_mut(&mut self) -> &mut SessionsPage {
        &mut self.sessions
    }

    /// Immutable sessions page.
    #[must_use]
    pub fn sessions(&self) -> &SessionsPage {
        &self.sessions
    }

    /// Mutable accounts page.
    pub fn accounts_mut(&mut self) -> &mut AccountsPage {
        &mut self.accounts
    }

    /// Immutable accounts page.
    #[must_use]
    pub fn accounts(&self) -> &AccountsPage {
        &self.accounts
    }

    /// Mutable Usage page.
    pub fn usage_mut(&mut self) -> &mut UsagePage {
        &mut self.usage
    }

    /// Immutable Usage page.
    #[must_use]
    pub fn usage(&self) -> &UsagePage {
        &self.usage
    }

    /// Mutable page by id (shell pages only).
    pub fn mut_page(&mut self, id: PageId) -> &mut PageShell {
        self.mut_shell(id)
            .expect("dedicated page is not a PageShell; use page_dyn or its typed accessor")
    }

    /// Draws a page as a trait object.
    #[must_use]
    pub fn page_dyn(&self, id: PageId) -> &dyn Page {
        match id {
            PageId::Agents => &self.agents,
            PageId::Providers => &self.providers,
            PageId::Routing => &self.routing,
            PageId::Usage => &self.usage,
            PageId::Sessions => &self.sessions,
            PageId::Library => &self.library,
            PageId::Accounts => &self.accounts,
        }
    }

    /// Immutable page by id (shell pages only).
    #[must_use]
    pub fn page(&self, id: PageId) -> &PageShell {
        match id {
            PageId::Agents => &self.agents,
            PageId::Providers => &self.providers,
            PageId::Routing => &self.routing,
            PageId::Library => &self.library,
            PageId::Sessions | PageId::Usage | PageId::Accounts => {
                panic!("dedicated page is not a PageShell; use page_dyn or its typed accessor")
            }
        }
    }

    /// Applies an HTTP probe result for deferred routes.
    pub fn apply_probe(&mut self, id: PageId, outcome: ProbeOutcome) {
        match id {
            PageId::Sessions => self.sessions.apply_probe(outcome),
            PageId::Library => {
                apply_probe_to_shell(self.mut_shell(id).expect("library shell"), outcome)
            }
            _ => {}
        }
    }
}
