//! Terminal UI for hofund, backed by the control client.

#![allow(missing_docs)]

pub mod app;
pub use app::AppAction;
pub mod events;
pub mod gateway;
pub mod intent;
pub mod pages;
pub mod quit;
pub mod redact;
pub mod runtime;
pub mod session;
pub mod state;
pub mod terminal;
pub mod theme;
pub mod ui;

pub use app::App;
pub use state::{Flash, Mode, PageId, PendingOp};
pub use theme::Theme;
pub use ui::buffer_text;
