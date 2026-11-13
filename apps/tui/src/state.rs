//! Shared application state.

use std::time::{Duration, Instant};

/// The six reference pages in navigation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PageId {
    /// Agents (key `1`).
    Agents,
    /// Providers (`2`).
    Providers,
    /// Routing groups (`3`).
    Routing,
    /// Usage (`4`).
    Usage,
    /// Sessions (`5`).
    Sessions,
    /// Library (`6`).
    Library,
}

impl PageId {
    /// All pages in `]` / `[` cycle order.
    pub const CYCLE: [Self; 6] = [
        Self::Agents,
        Self::Providers,
        Self::Routing,
        Self::Usage,
        Self::Sessions,
        Self::Library,
    ];

    /// Index `0`..=`5` for digit keys `1`..=`6`.
    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Agents),
            1 => Some(Self::Providers),
            2 => Some(Self::Routing),
            3 => Some(Self::Usage),
            4 => Some(Self::Sessions),
            5 => Some(Self::Library),
            _ => None,
        }
    }

    /// Stable slug for fixtures and headers.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Providers => "providers",
            Self::Routing => "routing",
            Self::Usage => "usage",
            Self::Sessions => "sessions",
            Self::Library => "library",
        }
    }

    /// Header label including the digit key.
    #[must_use]
    pub fn pill_label(self) -> String {
        let digit = Self::CYCLE
            .iter()
            .position(|page| *page == self)
            .map(|i| i + 1)
            .unwrap_or(1);
        format!("{digit} {}", self.slug())
    }
}

/// Top-level interaction mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Normal page view.
    #[default]
    List,
    /// Help placeholder (`?`, closed with `esc`).
    Help,
}

/// A short status-line message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flash {
    /// Message text.
    pub text: String,
    /// Whether the outcome was success.
    pub ok: bool,
    /// When the flash may be cleared automatically (optional).
    pub until: Option<Instant>,
}

impl Flash {
    /// Creates a flash without a timeout.
    #[must_use]
    pub fn new(text: impl Into<String>, ok: bool) -> Self {
        Self {
            text: text.into(),
            ok,
            until: None,
        }
    }

    /// Returns whether `now` is past the optional expiry.
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now >= until)
    }
}

/// One in-flight mutating control call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOp {
    /// Short label for the status line.
    pub label: String,
    /// When the operation started.
    pub started: Instant,
    /// Read generation of the mutation that took this slot.
    ///
    /// A completion may release the slot only when it carries this generation.
    /// A later mutation keeps the keyboard if an older completion arrives.
    pub generation: u64,
}

/// How this UI relates to a running daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayPresence {
    /// No client yet.
    Unknown,
    /// Attached to an existing daemon.
    Attached {
        /// Human-readable status.
        line: String,
    },
    /// This UI spawned the daemon.
    Spawned {
        /// Human-readable status.
        line: String,
        /// Child pid when known.
        pid: u32,
    },
    /// Could not reach or start a daemon; UI still open.
    Error(String),
}

impl GatewayPresence {
    /// Text for the status line when no flash is shown.
    #[must_use]
    pub fn status_line(&self) -> &str {
        match self {
            Self::Unknown => "starting…",
            Self::Attached { line } | Self::Spawned { line, .. } => line,
            Self::Error(message) => message,
        }
    }
}

/// Default flash duration when tests need a bounded lifetime.
pub const FLASH_TTL: Duration = Duration::from_secs(8);

/// Generation counter for in-flight reads.
pub type Generation = u64;
