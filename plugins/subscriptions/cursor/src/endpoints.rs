//! Cursor HTTP endpoints (overridable in tests).

/// Default Cursor API and website bases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Browser sign-in site.
    pub website: String,
    /// Connect API base (`api2`).
    pub api: String,
    /// Default agent API origin when server config has no usable override.
    pub agent: String,
    /// Client version sent on API calls.
    pub client_version: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            website: "https://cursor.com".to_owned(),
            api: "https://api2.cursor.sh".to_owned(),
            agent: "https://agentn.global.api5.cursor.sh".to_owned(),
            client_version: "cli-2026.09.23-86fc751".to_owned(),
        }
    }
}

impl Endpoints {
    pub(crate) fn login_deep_control(&self, challenge: &str, uuid: &str) -> String {
        format!(
            "{}/loginDeepControl?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli",
            self.website.trim_end_matches('/')
        )
    }

    pub(crate) fn auth_poll(&self, uuid: &str, verifier: &str) -> String {
        format!(
            "{}/auth/poll?uuid={uuid}&verifier={verifier}",
            self.api.trim_end_matches('/')
        )
    }

    pub(crate) fn oauth_refresh(&self) -> String {
        format!("{}/auth/refresh", self.api.trim_end_matches('/'))
    }

    pub(crate) fn server_config(&self) -> String {
        format!(
            "{}/aiserver.v1.ServerConfigService/GetServerConfig",
            self.api.trim_end_matches('/')
        )
    }

    pub(crate) fn exchange_user_api_key(&self) -> String {
        format!(
            "{}/auth/exchange_user_api_key",
            self.api.trim_end_matches('/')
        )
    }

    pub(crate) fn dashboard(&self, method: &str) -> String {
        format!(
            "{}/aiserver.v1.DashboardService/{method}",
            self.api.trim_end_matches('/')
        )
    }

    pub(crate) fn available_models(&self) -> String {
        format!(
            "{}/aiserver.v1.AiService/AvailableModels",
            self.api.trim_end_matches('/')
        )
    }

    pub(crate) fn usable_models(&self) -> String {
        format!(
            "{}/agent.v1.AgentService/GetUsableModels",
            self.api.trim_end_matches('/')
        )
    }
}
