//! WorkBuddy sites (China and international builds).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    China,
    International,
}

impl Site {
    pub(crate) const APP_VERSION: &'static str = "5.9.3";
    pub(crate) const CLI_VERSION: &'static str = "2.165.0";

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

    pub(crate) const fn app_name(self) -> &'static str {
        match self {
            Self::China => "WorkBuddy",
            Self::International => "workbuddy-ai",
        }
    }

    pub(crate) const fn user_agent(self) -> &'static str {
        "WorkBuddy/5.9.3"
    }

    pub(crate) fn cli_user_agent(self) -> String {
        format!(
            "WorkBuddy/{} {}/{} CLI/{}",
            Self::APP_VERSION,
            self.app_name(),
            Self::APP_VERSION,
            Self::CLI_VERSION
        )
    }
}
