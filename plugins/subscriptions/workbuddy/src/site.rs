//! WorkBuddy sites (China and international builds).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    China,
    International,
}

impl Site {
    pub(crate) const fn plugin_id(self) -> &'static str {
        match self {
            Self::China => "workbuddy",
            Self::International => "workbuddy-ai",
        }
    }

    pub(crate) const fn endpoint(self) -> &'static str {
        match self {
            Self::China => "https://copilot.tencent.com",
            Self::International => "https://www.workbuddy.ai",
        }
    }

    pub(crate) const fn auth_file(self) -> &'static str {
        match self {
            Self::China => "workbuddy-desktop.info",
            Self::International => "workbuddy-desktop-ai.info",
        }
    }

    pub(crate) const fn user_agent(self) -> &'static str {
        "WorkBuddy/5.5.6"
    }
}
