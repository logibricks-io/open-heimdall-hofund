//! Terminal UI shell for hofund (R-11).
//!
//! Page content lands in R-14 and follow-on rows; this crate is the shell only.

#![allow(missing_docs)]

pub mod app;
pub mod events;
pub mod gateway;
pub mod pages;
pub mod quit;
pub mod runtime;
pub mod state;
pub mod terminal;
pub mod theme;
pub mod ui;

pub use app::App;
pub use state::{Flash, Mode, PageId, PendingOp};
pub use theme::Theme;
pub use ui::buffer_text;
